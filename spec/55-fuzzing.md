# 55. Fuzzing (`fuzz/`, `decode.rs`, and every decoder it found)

A checksum (§40) proves a page's bytes are the ones that were written,
not that what wrote them was right, and §54 found that a page with a
wrong slot crashed the host application. §54 checked the slot directory;
this section looks for the rest by fuzzing. A fuzzer calls one entry
point millions of times with inputs it keeps changing, and keeps an
input that reaches code no earlier one did. A target's only rule is
that trunkdb doesn't panic or hang: any `Err` is a correct answer to
damaged input.

It found nine ways a damaged file crashed or hung trunkdb, and reading
the code around them found five more. All fourteen are fixed, and each
has a test in the library, so CI keeps them fixed without the fuzzer.

## 55.1 The fuzz crate
`fuzz/` is its own crate and workspace, like `bench/`, run with
`cargo-fuzz` on the nightly toolchain; the library stays on stable and
its MSRV. Four targets:

| Target | Input | Reaches |
|---|---|---|
| `open_file` | byte edits to a valid database, `[page][offset, u16][value]` each | every decoder of a page, cell, key and catalog entry, and the structures built from them |
| `open_raw` | any bytes as a whole database file | the header, and files of any length |
| `recover_wal` | a valid database, next to a WAL of any bytes or of one valid record of edited pages | the WAL's parsing, and recovery writing pages back |
| `import` | any bytes as JSON Lines, into a fresh database | the import; one that succeeds must pass `check` |

The valid database (`trunkdb_fuzz::base`) is built once per run, with
two collections, a document in overflow pages, a plain, a unique, a
compound, a sparse and two multikey indexes, branch pages, and free
pages from a dropped collection. After opening, a target makes the calls
the input's first byte chooses: `check`, finds through every kind of plan
(§34, §36, §43, §46, §49), an export, and writes followed by a
compaction.

**The `fuzzing` feature** gives the targets what the public API doesn't,
under `trunkdb::fuzzing`, hidden from the docs:
- `seal` and `unseal` add and remove each page's checksum. Without
  them, the fuzzer's bytes would almost never get past the checksums, and
  it would only ever test those.
- `wal` makes a valid WAL record of given pages.
- **No fsync.** `storage::sync` skips it in a fuzzing build: fuzzing
  tests decoding, not durability, and a flush per commit made each run
  slower.

**Seeds.** `cargo run --example seed_corpus` writes a whole database, a
valid WAL and an export into `corpus/`. Random bytes rarely get past a
file's magic number on their own.

**Speed.** The first version ran 12 inputs a second. Skipping fsync
gave little; the time was the work each run did (`check` 29%,
compaction 23%, finds 18%). Letting the input's first byte choose which
calls a run makes brought it to about 85. AddressSanitizer, which
`cargo fuzz` turns on by default, is off in these runs (`-s none`): the
library's only `unsafe` is the CRC intrinsics (§40), which get whole
slices, and the sanitizer cost a factor of four.

To run one: from `fuzz/`, `cargo run --example seed_corpus` once, then
`cargo +nightly fuzz run -s none open_file -- -max_total_time=600`. A
crash is saved under `artifacts/`, and replayed by passing its file
instead of the time.

## 55.2 What it found
In the order found. "Review" means found reading the code around a crash,
and fixed before the fuzzer got there.

| # | Where | What | How |
|---|---|---|---|
| 1 | `document.rs` | A document cut short panicked in `split_at`. Its comment called that a deliberate trust decision: nothing reads a cell but code that wrote it. | fuzzer |
| 2 | `document.rs` | A count or length near `u32::MAX` sized an allocation before the bytes behind it were read: gigabytes, then an abort. | review |
| 3 | `index/leaf.rs` | A leaf cell under 10 bytes panicked in `decode_index_entry` (predicted in the review before this section). | fuzzer |
| 4 | `index/key.rs` | A key shorter than an id, or a value cut short, panicked in the helpers that take keys apart. | fuzzer |
| 5 | `data.rs` | A data cell under 17 bytes, or an overflow cell under 29, panicked in `Cell::parse`. | fuzzer |
| 6 | `data.rs` | A damaged overflow length sized an allocation, and a chain looping back could be followed half a million times. | review |
| 7 | `catalog.rs` | A catalog cell cut short panicked. | review |
| 8 | `storage/file.rs` | A header counting pages past what a `u64` offset holds overflowed; one counting 0 would have handed out the header as a new page. | fuzzer |
| 9 | `storage/file.rs` | A WAL page id past the file's end was written there, or overflowed. | review |
| 10 | `index/btree.rs` | A child or sibling pointer back up the tree was followed forever. | review |
| 11 | `collection.rs` | A compound key with fewer values than the index has fields panicked when grouping a sorted read. | fuzzer |
| 12 | `storage/slotted.rs` | A page with no live slot and its cells "starting" past its end passed §54's check, and the next insert wrote there. | fuzzer |
| 13 | `catalog.rs` | Catalog pages linking in a loop hung `open`. | fuzzer (hang) |
| 14 | `storage/file.rs` | A free page linking past the end was followed by the next allocation. | fuzzer |

## 55.3 The fixes
- **`decode.rs`**: `take`, `take_u8`, `take_u32`, `take_u64`,
  `take_array` read from a `&mut &[u8]` and return `InvalidData` naming
  what ran out, never a panic. Document, data-cell and catalog decoding
  go through them. A count never sizes an allocation beyond the bytes
  left (each item is at least a byte), and an overflow document's
  buffer starts at 1 MiB at most.
- **B-tree pages** are read through `btree::read_node`, which checks the
  type and that every cell is long enough for its entry (10 bytes on a
  leaf, 8 on a branch). The entry decoders stay infallible, and none of
  their 14 callers changed.
- **Key helpers** in `index/key.rs` give a cautious answer to a damaged
  key: `doc_id` zero-pads, `value_part` is empty, `part_is_exact` says
  no, `parts` makes what's left one last part. They're used on keys read
  from the index, and every document is checked against the filter
  anyway (§28.3), so a wrong answer changes how much is read, not what
  matches.
- **Loops**: a chain of overflow pages, the catalog chain and the forward
  walk along the leaves keep the pages seen; a page seen twice is an
  error. Descending the B-tree (`find_leaf`, the walk, insert) stops at
  64 levels; a page holds at least four entries, so 28 would already be
  more pages than a file can have.
- **The file**: the header must count at least itself, at most what a
  `u64` offset holds, and its free list must start inside the file; each
  free-list link is checked when allocation follows it; WAL pages must
  lie inside the file as the header before them has it (a batch that
  grows the file logs its header too, as page 0, first), checked before
  anything is written.
- **Pages**: the cells must start inside the page (the check §54 lacked).

Rejected:
- **Every decoder returning `Result`**, B-tree entries included. It's
  the more uniform answer, but the entry decoders have 14 callers, many
  in iterator chains, and one length check per page read makes them
  safe without touching any.
- **Fallible key helpers.** Twenty callers, for damage that can only
  change how much a query reads. Finding it is `check`'s job (§39).
- **A cap on document nesting when decoding**, here: it also limits
  what may be written, so it got its own section (§56).

## 55.4 What it costs
The same machine, the same day, `bench` (§48) against the commit before:
lookups within their run-to-run spread (5.3–7.4 µs against 6.6–6.7),
the full scan about 6% slower (156 ms against 144–148, the checked
document decoding), batched inserts 22–23k/s against 24k/s.

## 55.5 Tests
- One regression test per fix, in the library, so CI runs them without
  the fuzzer:
  - `document.rs`: every prefix of an encoded document of every type is
    an error; counts and lengths of `u32::MAX` are errors.
  - `index/btree.rs`: a cell too short, in a leaf or a branch, is an error
    from lookup, scan, insert, `pages` and a walk; a root that is its own
    last child, and a last leaf linking to the first, are errors.
  - `index/key.rs`: every prefix of a compound key gets an answer from
    every helper, and `parts` covers exactly its bytes.
  - `data.rs`: every prefix of an inline and an overflow cell's fixed
    part is an error; a chain looping to itself with a length of
    `u32::MAX` is reported at once.
  - `catalog.rs`: every prefix of the three kinds of cell decodes or is
    an error, and always the error within the fixed part; a catalog page
    linking to itself is an error.
  - `storage/file.rs`: headers counting 0 or too many pages, or a free
    list past the end, are refused; WAL pages past the end are refused
    with the file untouched, one the WAL's header makes room for is
    restored; a free link past the end is an error.
  - `storage/slotted.rs`: cells starting past the page's end.
  - `collection.rs`: grouping keys with values missing.
  - `decode.rs`: `take` moves past what it takes and refuses what isn't
    there, moving nothing.
- The last round ran each target 10 minutes, about 690,000 runs in all,
  with no crash or hang.
- Checked by breaking it on purpose, fifteen ways. Each of these fails a
  test:
  - `take` never refusing;
  - an array's count sizing its allocation;
  - no minimum B-tree cell length;
  - `doc_id` or `is_other` trusting the length; `parts` running past
    the end;
  - a header of 0 pages, or one whose offsets overflow; a free list
    starting past the end;
  - WAL pages past the end; a free link past the end;
  - cells past the page's end;
  - an insert without the depth limit;
  - groups trusting the value count;
  - a data cell's id unchecked.

  Not broken on purpose: the loop guards on overflow chains, the leaf
  walk and the catalog chain. Without them a test doesn't fail, it
  grows memory without limit. Each has a test built from a real loop.

## 55.6 Limits
- **Deep nesting.** Decoding a document recurses once per level, and so
  does encoding one. A damaged overflow document could claim tens of
  thousands of levels and overflow the stack. The fuzzer can't reach it
  (its edits stay within a page), and a cap would also refuse inserting
  such a document, which is an API decision: made in §56, a limit of 64
  levels.
- **Overlapping cells** (§54.2) and other damage to what cells mean — a
  B-tree page with its keys out of order, a key whose id part changed —
  aren't found on read. They give errors or wrong answers, not panics;
  `check` (§39) looks for them.
- **CI doesn't fuzz.** It builds and lints the fuzz crate on stable, so
  the targets keep compiling. Fuzzing needs nightly and minutes to
  hours; runs are by hand.
- **Short so far**: about an hour of fuzzing per target in all, without
  AddressSanitizer. Coverage was still growing at the end.
