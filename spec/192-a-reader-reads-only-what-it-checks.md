# 192. A reader reads only what it checks (`durability/wal.rs`)

§190.6 left an open unmeasured. Measured now, on a 100,000-task file
(24 MB, three indexes, nothing to recover), it turned out cheap. But a
read-only open beside a writer did 79 µs a lookup once the WAL held
anything, 1 batch or 50, where it did 15 µs beside an empty one. That
was §188's catch-up reading far more than it needed on every call.

## 192.1 What it read
Each call, `read_pending_since` (§188.2, step 2) opened the WAL and
wrapped it in a `BufReader` of 1 MiB (`READ_BUFFER`). Then:

1. it read the WAL's header, 44 bytes: the buffer filled itself from
   the file, up to a megabyte;
2. for each record of the last batch it read, it seeked to the record
   and read its 8 bytes: a seek throws away what a `BufReader` read
   ahead, so each of these read up to a megabyte again;
3. it seeked to where it stopped, and read what the WAL had gained.

A call that found nothing new read up to 1 MiB for the header and up
to another for each check, to compare 44 + 8 bytes a record. A
profile (`sample`) had 75% of the time in `read`, all of it under
`read_pending_since`.

## 192.2 The change
The header and the checks are read from the file as they are, without
the buffer, each a read of its own; the buffer starts where the reader
stopped, for what the WAL gained. Nothing else changed: what is read,
checked and caught up is as §188 has it.

Sizing the buffer to what the WAL gained, so a call with little new
doesn't allocate a megabyte, was tried too and dropped: it measured the
same.

## 192.3 Measured
Apple M1 Pro, macOS, release build, 2026-11-13. A scratch program,
outside the repository: 100,000 tasks with the bench's three indexes
(§48), checkpointed; the writer a process of its own, holding the WAL
with batches of 1,000 inserts it doesn't checkpoint. Medians:

| | open | first `get` | close | each `get` after |
|---|---:|---:|---:|---:|
| a writer, alone | 4.6 ms | 28 µs | 4.4 ms | 1.6 µs |
| read-only, alone | 0.14 ms | 53 µs | 0.007 ms | 15 µs |
| read-only beside a writer, its WAL empty | 0.14 ms | 53 µs | 0.007 ms | 15 µs |
| … the WAL holding 1 batch | 1.1 ms | 1.1 ms | 0.03 ms | 79 µs → 18 µs |
| … 10 batches | 7.6 ms | 7.5 ms | 0.13 ms | 79 µs → 18 µs |
| … 50 batches | 34.5 ms | 36 ms | 1.3 ms | 79 µs → 18 µs |
| a writer, beside an idle reader | 5.1 ms | 35 µs | 4.4 ms | — |

- **A writer's open and close** take about one flush each, 4.5 ms on
  this machine; a reader beside it changes little.
- **A read-only open** costs 0.14 ms; its first call reads what the WAL
  holds into memory, about 0.7 ms a batch of 1,000 inserts, and later
  calls only what is new (§188).
- **A call on a read-only open** costs about 15 µs on top of the lookup,
  writer or not: the gate passed, the readers' lock taken and let go,
  the file's header and the WAL looked at (§187, §188). Now 3 µs more
  with a WAL to check, for its header and records read one by one.
- Opening the WAL anew on every call is most of what's left; keeping it
  open would need the reader to notice a WAL removed and made again.
  Not taken further: 15 µs is well within what an admin tool or an
  agent needs.

## 192.4 Tests
- `durability/wal.rs`: `a_reader_reads_only_what_it_checks_and_what_was_gained`.
  A WAL of more than 2 MiB whose last batch has two records; a call
  with nothing new reads at most its header and 8 bytes a record, and
  one after a new batch that and the batch's bytes. In tests the WAL is
  read through `Counted`, which counts the bytes per thread.
- The tests of §188 pass unchanged.

## 192.5 Mutations
| Mutation | Test |
|---|---|
| the buffer in front of the header and the checks, as before | caught |
| the buffer in front of the checks only | caught |
