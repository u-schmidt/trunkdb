# 51. One flush per commit (`database.rs`, `storage/file.rs`)

Real and measured: a commit now flushes once, the WAL. Its pages are
written back to the main file later, at a checkpoint, together with
those of the commits before it. One commit went from 16 ms to 4.6–4.8 ms in
the benchmark (§48), level with SQLite and redb (4.6–5.6 ms across
runs) and twice as fast as sled. No file or WAL format changed.

## 51.1 The commit protocol, again
§19.3's steps 4 and 5 moved out of the commit:
1. **stage**, 2. **apply**, 3. **log** — as before; the WAL's `fsync` is
   the one flush a commit waits for.
4. **commit** (`FileStore::commit`): the staged pages join the
   *unwritten* pages, committed and durable in the WAL but not in the
   main file, with the newest image of each. Nothing touches the file.
5. **checkpoint**, only once 1,000 or more pages are unwritten (8 MB,
   SQLite's default for its WAL mode too): write them all back, `fsync`
   the main file, truncate the WAL. Also at `Database::checkpoint()`,
   which is new and public, and when the last handle is dropped.

A read looks in the batch's staged pages, then the unwritten ones, then
the cache (§50), then the file. A page allocated past the file's end
lives only in the unwritten pages until the checkpoint writes it. The
main file lags behind the WAL between checkpoints, and the WAL holds
every batch since the last one, in order.

Recovery doesn't change (§19.4): it writes back every complete record in
the WAL, in log order, so each page ends at its latest image. That was
already how it handled several records, which a failed checkpoint could
leave behind (§19.6).

## 51.2 Why not three flushes to two first
The plan in §48.4 was to drop only the flush after truncating the WAL
first, and it doesn't survive a closer look. If the truncation isn't
durable, a power cut during the *next* commit's log can leave that
commit's partial record written over the start of the old record. The
old record then fails its checksum, but it isn't the last record: bytes
of the old record follow it. §19.3 treats a checksum mismatch before the
tail as corruption, not as a torn write, so the database would refuse to
open. Making that safe would need a WAL format that recognizes stale
records (SQLite stamps each frame with a salt that changes at every
checkpoint).

Checkpointing less often gets to one flush without that. The truncation
keeps its flush, and it's paid once per checkpoint, not per commit.

## 51.3 Failures got simpler
- **A checkpoint that fails** (a full disk, say) may leave the main file
  half-written. Nothing reads those pages from the file, though: they're
  all still unwritten, and reads find them there first. So the pages
  stay, the WAL isn't truncated, and the next checkpoint writes them all
  again. The batch that triggered it succeeds, because it is durable.
  Before, a write-back that failed twice poisoned the database (§19.6).
- **The WAL is truncated only after the checkpoint's own flush.** If the
  truncation fails, the next open writes the records back once more,
  which is harmless.
- **Poisoning** remains for a failed `log` that can't be undone (§19.6),
  and for a panic mid-batch (§27.3).
- **`check`** skips unwritten pages in its damaged-page scan (§50.3):
  their file copy is older or missing, and the WAL holds them.

## 51.4 What it costs
- **Memory:** up to 1,000 unwritten pages (8 MB), plus whatever one
  batch adds past that before its checkpoint. A compaction's whole image
  goes through them and is checkpointed at once (§41).
- **The main file isn't complete on its own between checkpoints.** A
  backup that copies only the `.trunkdb` file needs `db.checkpoint()`
  first, or the WAL too. Export (§30) needs neither.
- **The commit that crosses the threshold pays for the checkpoint:**
  writing back up to 8 MB and two flushes. Batches of 1,000 documents
  cross it every few commits, which is why batched writes gained less
  (6,800 to 7,500 per second). They're bound by page work, not flushes
  (§48.4).

Rejected:
- **A checkpoint thread.** It would take commits' checkpoints off their
  own time, but it adds a second writer to coordinate with, which is
  exactly what §27 avoids. A synchronous checkpoint every 8 MB is
  predictable.
- **A per-page index into the WAL file**, as SQLite keeps, instead of
  the pages in memory. It would save the 8 MB, but it costs a file read
  per unwritten page read and a format for the index. The cache already
  holds pages in memory; these are the same kind.

## 51.5 Measured
100,000 documents, trunkdb alone:

| | before | after, two runs |
|---|---:|---:|
| insert 1000, one per commit | 16,158 µs | 4,623–4,792 µs |
| insert 100000, 1000 per commit | 6,800/s | 7,504–7,518/s |
| update, 1000 per commit | 5,852/s | 6,187–6,409/s |
| delete, 1000 per commit | 11k/s | 14–15k/s |

Reads stayed within the run-to-run spread. The full run, with the
others:

| | trunkdb | SQLite | redb | sled |
|---|---:|---:|---:|---:|
| insert 100000, 1000 per commit | 7518/s | 39k/s | 43k/s | 30k/s |
| insert 1000, one per commit | 4623 µs | 4565 µs | 4835 µs | 9536 µs |
| get by id | 3.9 µs | 4.4 µs | 1.8 µs | 2.2 µs |
| find tenant == x (1000 docs) | 2.37 ms | 1.66 ms | 1.04 ms | 2.40 ms |
| status == x, oldest 20 | 47.0 µs | 22.8 µs | 21.7 µs | 20.3 µs |
| scan: tries > 7, unindexed | 180 ms | 43 ms | 52 ms | 67 ms |
| update 9528, 1000 per commit | 6187/s | 13k/s | 18k/s | 24k/s |
| delete 10000, 1000 per commit | 14k/s | 19k/s | 22k/s | 36k/s |

SQLite's lookup came out at 4.4 µs in this run and 16–18 µs in every
earlier one; the small figures move between runs. The test suite runs in 16
seconds instead of 26, since most of its tests commit.

## 51.6 Tests
- `database.rs`:
  - A commit leaves the main file as it was and the WAL non-empty; the
    batch is readable; `checkpoint` writes it back and empties the WAL.
  - Commits of 50 pages each until one crosses 1,000 pages: that commit
    checkpoints by itself, and the WAL is empty after it.
  - Dropping one of two handles keeps the WAL; dropping the last
    empties it, and the file opens complete without it.
  - A failed checkpoint keeps the batch readable and the WAL full, and
    the next checkpoint writes it back.
  - Checkpoints that keep failing, the one at drop included: the next
    open restores the batch.
  - Three batches rewriting the same 30 one-page documents, with the
    checkpoint and the one at drop cut short after 0, 1, 3, 7 or 20
    pages: the next open has the last batch's values everywhere, and
    `check` finds nothing.
- `storage/file.rs`:
  - Committed pages are read before the checkpoint writes them,
    including a page past the file's end; `damaged_pages` skips them.
  - A checkpoint that failed halfway: reads still say the committed
    pages; the next checkpoint writes all of them, and the cache holds
    them after.
- `compact.rs`: a compaction whose checkpoints fail is still committed,
  readable and checked clean; the next open completes it and cuts the
  file.
- The recovery tests of §19 run unchanged: they drive the protocol by
  hand, with a full write-back where the crash point asks for it.
- Checked by breaking it on purpose, ten ways. Each of these fails a
  test:
  - a commit never checkpointing;
  - reads skipping the unwritten pages, in either read path;
  - the damaged-page scan reading them from the file;
  - a checkpoint not cutting the file, or keeping its pages unwritten
    after writing them;
  - the WAL truncated before the checkpoint's write-back, or even if it
    failed;
  - no checkpoint when the last handle goes;
  - an older image of a page kept over a newer one.
