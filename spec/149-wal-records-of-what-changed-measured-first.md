# 149. WAL records of what changed, measured first (`durability/wal.rs`, `storage/file.rs`)

**A draft, 2026-11-02: measured and designed, not built.** Whether to
build it is open (149.8).

§123.5 put this first among what would close the gap on batched
writes: "10 MB written and flushed per commit for 200 KB of documents".
This section measures what records of the changed bytes would save
before anything is built, and finds less than §123.5 expected.

## 149.1 What the WAL writes now
Every page a batch changes goes to the WAL whole, 8,188 bytes and its
id (§19.2). After a checkpoint a page's first image has to stay whole:
a checkpoint can tear a page in the file, and only a whole image
repairs it. Every later image of it before the next checkpoint could be
the byte ranges that changed since the one before.

## 149.2 The probe
A build kept in a scratch copy, not in the repository:
- **Counting:** `log_each` keeps the last image of each page it logged
  since the last checkpoint, and compares each new image with it. A
  page logged for the first time since a checkpoint counts whole, as
  does the header page always (§85 and §87 check its images). A page
  logged again counts as a range entry: `[u64 id][u8 kind][u16 count]`,
  then `[u16 offset][u16 len][bytes]` per range, with ranges closer
  than 8 bytes merged, and never more than a whole entry. A checkpoint
  forgets every image.
- **Timing:** with `TRUNKDB_PROBE_WRITE=small`, each record is cut to
  the bytes its range entries would take before it is written and
  flushed. Such a WAL can't be recovered, which the benchmark never
  does: its checkpoints write back from memory. What it doesn't time is
  the CPU a real build would save or spend: the record is still built
  and checksummed whole, and the comparison is done in both modes.
- The §48 benchmark, trunkdb alone, 100,000 tasks, on the machine of
  §123, against `a98a79c`.

## 149.3 Bytes
Through the benchmark's write phases, at the default WAL limit
(65.5 MB, §76):

| | page images | of them first since a checkpoint | whole | as ranges | fewer |
|---|---:|---:|---:|---:|---:|
| insert 100,000, 1,000 a commit | 84,285 | 14,384 (17%) | 691 MB | 164 MB | 4.2× |
| update 9,528, 1,000 a commit | 21,484 | 10,527 (49%) | 176 MB | 105 MB | 1.7× |
| delete 10,000, 1,000 a commit | 16,444 | 6,110 (37%) | 135 MB | 54 MB | 2.5× |

- **A page logged again changes little:** 625 bytes on average, in six
  ranges. Mostly the slot directory shifting behind an inserted cell
  (`insert_cell_at`, §52), the cell, and the header.
- **First images are most of what would remain:** on inserts, 118 of
  the 164 MB. Updates and deletes reach pages no recent commit touched
  (data pages, the leaves of documents picked at random), so half of
  their images are first ones.
- **Checkpoints decide how many first images there are,** and the WAL
  limit decided the checkpoints: with whole images, 11 of the 100
  insert commits checkpointed. §123.1 put that on the 4,000 waiting
  pages; at 6.9 MB a commit, it is the 65.5 MB limit that is crossed
  first. With the limit at 1 GB only two did, and 6,311 images were
  first ones: 103 MB as ranges, 6.7× fewer.

## 149.4 Time
Two runs of each:

| | WAL limit | checkpoints in the inserts | insert | update | delete |
|---|---|---:|---:|---:|---:|
| whole images | 65.5 MB | 11 | 29k/s, 29k/s | 12k/s, 12k/s | 22k/s, 23k/s |
| ranges' bytes | 65.5 MB | 2 | 33k/s, 34k/s | 13k/s, 13k/s | 25k/s, 26k/s |
| whole images | 1 GB | 2 | 30k/s, 31k/s | 13k/s, 13k/s | 24k/s, 24k/s |
| ranges' bytes | 1 GB | 2 | 34k/s, 32k/s | 15k/s, 13k/s | 26k/s, 26k/s |

The probe's own work costs about 7%: the build without it measured
31k/s, 12k/s and 25k/s the day before.

- **About 15% on inserts,** of which fewer bytes give 7–10% and fewer
  checkpoints the rest. Updates and deletes gain 5–15%.
- **A single commit doesn't move:** 4.9–5.2 ms in every run, one
  flush of a few pages either way.

## 149.5 Where the rest goes
A commit of 1,000 inserts takes about 32 ms. §123.1 measured the same
build with every flush skipped at 41–47k/s, about 22 ms a commit: the
CPU's part. Of the 10 ms spent waiting on the disk, records of ranges
would save about 3. The rest is the flush itself, which a commit of one
page pays too, and the checkpoints.

What a real build would save on the CPU is not in the table. §123.1's
profile gave writing the WAL record 17% of the CPU, about 3.7 ms a
commit. Range records would copy and checksum a quarter of the bytes,
but comparing each page with its image before costs a pass over both;
perhaps 2 ms saved. With that, inserts at about 35–37k/s, 1.15–1.2×
now, and still under SQLite's 40–44k/s.

So §123.5's order was wrong: the CPU, not the WAL's bytes, is most of
what is behind. 75% of a commit is the B-tree and the encoding,
allocation (15%), `check_layout` (6%) and the copy of a page on its
first change (6%).

## 149.6 If built: the design
- **The image a range is taken from is the batch's `before`:**
  `Staging::before` already holds each page as the batch found it, the
  committed version, for snapshots (§80). For a page whose whole image
  is in the WAL since the last checkpoint, that is the image the WAL
  last logged of it, so ranges against it rebuild the new page from
  what the WAL holds. No second copy is kept.
- **`WalDurability` knows which pages are whole in the WAL:** a set of
  ids, about 8 bytes a page, at most the waiting pages and one batch.
  A page not in it is logged whole and goes in. The header page, a
  compaction's image (§108) and a page past the old end are always
  whole.
- **A checkpoint empties the set when it cuts the file,** before its
  flush, which may fail (§128): an empty set only makes the next images
  whole.
- **`undo_failed_log` takes back what the failed log added to the
  set:** a record cut away may have held a page's first whole image,
  and a range against it in the next batch would have nothing to apply
  to.
- **Recovery restores in log order, as now (§91):** a whole image is
  written to its page; a range entry is applied to the page as restored
  so far, read back from the file. Its whole image was written earlier
  in the same pass, so a page the crash tore is never the base. A range
  entry whose page has no whole image before it in the WAL is an
  error, never applied to the file's page. A read-only open (§88)
  applies ranges onto the images it keeps in memory.
- **WAL version 4:** entries of two kinds, a body of variable length
  (the length prefix already gives it; §91's limit of 32 MiB of pages a
  record becomes one of bytes). Versions 2 and 3 are still read, and
  never added to (§91). An older build meeting a version-4 WAL a crash
  left refuses it clearly, as with every WAL version before. The file
  format doesn't change, and after a clean close the WAL is empty.
- **The fuzz target `recover_wal`** learns range entries, with ranges
  out of the page, overlapping, and without a whole image before.

## 149.7 Rejected
- **A WAL of operations:** §16 had one, and §19.1 replaced it because
  its replay couldn't repair a structure a crash left half written, a
  B-tree split with only some of its pages on disk.
- **Ranges for a page's first image, against the file's page:** the
  page a checkpoint may tear (149.1).
- **Compressing the records:** CPU spent where CPU is what is behind.
- **4 KiB pages:** §123.7.

## 149.8 Open: build it or not
For about 1.15–1.2× on batched writes, it changes the code a crash
depends on most: recovery, the undo of a failed log (§81, §128), and
the WAL's format. Nothing it does helps a single commit.

The alternative: profile the CPU part again, as §123.1 did, and take
what it shows first: `check_layout` for a node already checked when it
entered the cache, the copy on first change, allocations on the write
path. Those change no format and no recovery. Range records would stay
in ROADMAP.md with this section's numbers, for when a program's writes
ask for them.

## 149.9 Tests, if built
- A page logged whole, then as ranges in later batches, recovered to
  its last state; the same after a checkpoint, when it is whole again.
- A batch whose log fails after a page's first whole image, then a
  batch that changes the page again: the second logs it whole.
- A checkpoint that cuts the file and then fails (§128): the next
  images are whole.
- A crash during a checkpoint's write-back, with a page torn in the
  file: recovery rebuilds it from its whole image and the ranges.
- A WAL of version 4 with a range entry and no whole image before it:
  an error at open, the file untouched.
- WALs of versions 2 and 3 still recovered (§91, §120's fixtures).
- Mutations to try: the set not emptied at a checkpoint; the undo
  leaving the set as it was; ranges against the file's page instead of
  `before`; a range entry applied with no whole image before it.
