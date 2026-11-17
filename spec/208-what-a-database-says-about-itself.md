# 208. What a database says about itself (`open_info.rs`, `database.rs`, `reports.rs`, `trunkdb-ffi/`, `bindings/`)

A `Database` couldn't say which file it works on, whether it was
opened read-only, or what options it uses: `stats`, `file_info` and
`snapshot_info` say what is in the file and what snapshots cost, not
the open. A program logging which file it opened, an admin screen, or
an error message had to keep the path and the options beside the
handle itself. The open knew all of it.

## 208.1 The calls
```rust
db.path()          // &Path: the file, its symlinks followed
db.is_read_only()  // bool
let info = db.open_info();
info.path; info.wal_path; info.mode; info.poisoned; info.options
```
- **`path`:** the path given to the open, its symlinks followed to the
  file they lead to (§127): the file read and written, the one the WAL
  and the lock files are named from. Only the last part is followed,
  as the open follows it; the directories are as given.
- **`wal_path`:** `<path>.wal`.
- **`mode`, an `OpenMode`:** `Writer`; `ReadOnlyBesideWriter`, by the
  lock files (§187), each call reading what a writer committed, one
  open now or not; `ReadOnlyAlone`, holding the file shared (§88),
  reading it as it was at the open. `is_read_only` is every mode but
  `Writer`.
- **`poisoned`:** a batch logged but not written to the file, even on
  retry (§19.6): every other call fails with `Error::Poisoned` until
  the database is opened again. `open_info` answers then too.
- **`options`, an `OpenSettings`:** each `OpenOptions` value as the open
  uses it, the default for one not set: `cache_size`,
  `checkpoint_pages`, `checkpoint_wal_bytes` (twice the pages' bytes
  unless set), `snapshot_memory`, `checkpoint_wait`,
  `wait_for_readers`, `let_go_wal_bytes` (four times the WAL's bytes
  unless set; `None` where 0 says never), `watch_every`, `clock_every`.
- All but `poisoned` are fixed at the open: kept in `Shared` and read
  without a lock. `poisoned` is read as asked.
- `OpenInfo`, `OpenMode` and `OpenSettings` are `#[non_exhaustive]`, so
  a field or a mode can be added without breaking a program.

## 208.2 As a document, and in the bindings
- **`OpenInfo::to_document`** (§198): `{"path", "wal_path", "mode":
  "writer" | "read_only_beside_writer" | "read_only_alone", "read_only",
  "poisoned", "options": {…}}`, each duration in seconds as a float,
  as Python's `watch_every` takes one; a path that isn't UTF-8 with
  U+FFFD for what isn't.
- **C ABI:** `trunkdb_open_info(db, &report)`, that document as JSON.
  `TRUNKDB_ABI_VERSION` 3: a function added, as §194.4 raised it to 2,
  so the C# wrapper refuses a library without it at `Open`.
- **C#:** `db.Path`, `db.IsReadOnly`, asked once and kept, and
  `db.OpenInfo()`: an `OpenInfo` record, its `Mode` an `OpenMode` enum,
  its options' durations `TimeSpan`s.
- **Python:** `db.path` (a `str`) and `db.read_only`, properties, and
  `db.open_info()`, a dict; a closed database raises, as for every call.

## 208.3 Decided with the user
- Accessors for the common questions and one report for the rest,
  beside `stats`, `file_info` and `snapshot_info`.
- The file, its symlinks followed, not the path as given: the file
  worked on.
- The options reported back.
- The mode only: not whether a writer in another process is open now.

## 208.4 Rejected
- **Whether a writer is open now,** for a read-only open beside one: a
  probe of its lock, which says what was true a moment ago. The mode
  says how the open reads; a program that needs the writer asks it.
- **Counts of live queries, aggregates and subscribers:** a dropped one
  is noticed at its collection's next change (§135, §138), so a count
  would include those gone until then.
- **Getters on `OpenOptions`:** `cache_size(self, n)` is its setter, and
  Rust has no overloading; a getter wants another name for each. The
  report's fields, plain and public, read as a C# property does, and
  `OpenOptions` keeps one name per option.
- **The path as given beside the file:** the file is what is worked
  on; a program that wants its own spelling has it.
- **The path in `Database`:** every clone would copy it. It is the
  open's, kept once in `Shared`.

## 208.5 Tests
- `tests/open_info.rs`: a writer, its clone the same; the options set
  and those not set as used; WAL bytes set and readers never asked;
  through a symlink, the file it leads to (Unix); read-only beside a
  writer and after it closed; read-only alone, in a directory it may
  not write (Unix, not as root); the document.
- `database.rs`: a poisoned database says so, before and after, where
  every other call refuses.
- `trunkdb-ffi`: `trunkdb_open_info` for a writer and a reader beside
  it, a null report refused; `every_function.c` calls it.
- C#: `A_database_says_its_file_mode_and_options`; the ABI version 3.
- Python: `test_a_database_says_its_file_mode_and_options`, and a
  closed database's `path` raising.

## 208.6 Mutations
| Mutation | Caught by |
|---|---|
| Every read-only open reported alone | read-only beside a writer, C ABI, C#, Python |
| Every read-only open reported beside a writer | read-only alone |
| `is_read_only` true for a writer | a writer, C#, Python |
| The path as given, not followed | through a symlink |
| `poisoned` never set | the poisoned test |
| The WAL's default bytes not worked out | the options as used |
| `let_go_wal_bytes` 0 reported as a number | readers never asked |
| A duration in milliseconds in the document | the document, C ABI, C#, Python |
