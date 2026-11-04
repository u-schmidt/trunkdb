<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/trunkdb-logo-dark.svg">
    <img src="assets/trunkdb-logo-light.svg" alt="trunkdb" width="260">
  </picture>
</h1>

[![CI](https://github.com/u-schmidt/trunkdb/actions/workflows/ci.yml/badge.svg)](https://github.com/u-schmidt/trunkdb/actions/workflows/ci.yml)

An embedded, single-file, schema-less document database in Rust. Open a
file, get collections of documents: no server, no tables, no joins, no
schema migrations. Any `T: Serialize + Deserialize` is a document.

It started as a way to get [LiteDB](https://www.litedb.org/)'s easy
embedded document store outside .NET, and to learn how a database is
built, from the page layer up. It has grown well past that start,
and it still grows the same way: each step is a
numbered [spec](spec/README.md) section with its reasons and its
measurements.

## Features

- Typed documents through serde, or untyped ones (`Document`).
- Filters in code or as text, with sort, skip and limit, parameters for
  values passed apart from the text, and conditions of your own as
  closures; projection and
  aggregates by group, with a condition per column. Arrays, objects and binary compare by their
  contents with `=` and `!=`.
- Secondary indexes: unique, compound, on nested paths and array
  elements, and sparse; a hint in the query names the one to read.
- Atomic batches across collections, with preconditions checked at
  the commit; every commit crash-safe through a write-ahead log, and
  reported to subscribers once it is; live queries that report what
  comes to match a filter and what leaves it, and live aggregates over
  a window of the last minutes.
- Readers in parallel beside one writer, and snapshots for reads that
  must agree.
- `explain_run` and `stats` to see how a query ran, what the plans it
  passed over would have read, and where the space goes; `check`,
  `compact`, export and import, and a backup checked by restoring it; a
  `trunkdb` command line.

## When to use it, and when not

trunkdb fits where SQLite would, but the data is documents rather than
rows: desktop and command-line apps, local caches, tools, small
services, test data. One file, no setup, nothing to run beside it.

It isn't the right choice (yet) for:

- Several processes on one file, or several writers at once.
- Encryption or schema validation in the database.
- Joins, or a cost-based planner: the planner follows fixed rules, and
  `explain` says which one it took.
- A file format fixed for good: a new version may need an export and
  import to move a file up ([ROADMAP.md](ROADMAP.md)).

## Getting started

trunkdb isn't on crates.io yet. Add it from git:

```toml
[dependencies]
trunkdb = { git = "https://github.com/u-schmidt/trunkdb" }
serde = { version = "1", features = ["derive"] }
```

Pinning a commit with `rev = "…"` keeps updates in your hands;
[CHANGELOG.md](CHANGELOG.md) lists what each release changes.

```rust
use serde::{Deserialize, Serialize};
use trunkdb::{Database, DocId, query::Filter};

#[derive(Serialize, Deserialize)]
struct User {
    #[serde(rename = "_id")]
    id: Option<DocId>, // None on insert: one is made; filled in on every read
    name: String,
    team: String,
    age: i64,
}

let db = Database::open("app.trunkdb")?;
let users = db.collection::<User>("users");
users.ensure_index(["team", "age"])?; // once at startup; a no-op after

let id = users.insert(User { id: None, name: "Ada".into(), team: "core".into(), age: 36 })?;
let ada = users.get(&id)?;

let youngest = users.find(Filter::new().eq("team", "core").sort_asc("age").limit(5))?;
let same: Filter = r#"team = "core" order by age limit 5"#.parse()?;
```

[GUIDE.md](GUIDE.md) goes on from here: indexes, updates, batches,
snapshots, selecting fields and aggregates.

## Command line

```
cargo install --git https://github.com/u-schmidt/trunkdb
trunkdb info app.trunkdb                 # format, releases, pages and sizes, collections, indexes
trunkdb check app.trunkdb                # consistency check; exit code 1 on problems
trunkdb compact app.trunkdb              # rebuild into as few pages as needed
trunkdb export app.trunkdb backup.jsonl  # JSON Lines, or to standard output
trunkdb import copy.trunkdb backup.jsonl # `-` reads standard input
trunkdb verify backup.jsonl              # whether it restores: tried, checked, removed
trunkdb find app.trunkdb users 'age >= 30 order by name limit 10'
                                         # one JSON document per line
trunkdb find app.trunkdb users 'age >= 30' --explain
                                         # the plan, index reads, counts
trunkdb fields app.trunkdb users         # every path, in how many documents,
                                         # as which types
trunkdb count app.trunkdb users 'age >= 30'
trunkdb find app.trunkdb users 'name = :name' --param "name=$NAME"
                                         # a value apart from the text
trunkdb get app.trunkdb users 0192f0c4-7d7a-7c43-9e0b-1f6d1b8f0a11
trunkdb aggregate app.trunkdb users 'age >= 30' 'count() as n, avg(age) as mean group by team'
trunkdb insert app.trunkdb users '{"name": "Ada", "age": 36}'
trunkdb update app.trunkdb users 'name = "Ada"' '{"$inc": {"age": 1}}'
trunkdb delete app.trunkdb users 'age < 18' --dry-run
trunkdb index app.trunkdb users add team age
trunkdb drop app.trunkdb old_users
```

`info`, `check`, `export`, `find`, `fields`, `count`, `get` and
`aggregate` open the file read-only; `info`, `check` and `fields` print JSON with `--json`.
An update or delete that would change every document of a collection
asks for `--all`, and `--dry-run` says how many it would change.
`trunkdb help` lists every command and option.

### For an AI agent

[skills/trunkdb/](skills/trunkdb/SKILL.md) is an
[Agent Skill](https://agentskills.io) that teaches an AI agent with a
shell (Claude Code, Codex, Gemini CLI and others) to look into a
trunkdb file with this command, and change it: shape first, then
narrow queries, and never the whole file into its context; a write
counted with `--dry-run` and confirmed before it runs. Copy the folder
to where your agent reads skills. The file must not be open in another
program meanwhile: one that writes holds it locked.

### A desktop app

[TrunkDB Admin](https://github.com/u-schmidt/trunkdb-admin) looks into
and manages a trunkdb file without code: collections and indexes,
documents as JSON or as a table, filters as rows or as text, groups,
explain, editing, check, export and import. It is a project of its own,
made on macOS, and bundles a trunkdb release; while it has a file open,
it holds the file locked.

## Performance

`bench/` runs one document workload against trunkdb, SQLite, redb and
sled, with the same indexes, durable commits everywhere and the same
answers checked across all four. 100,000 documents on an M1 Pro, the
median of three runs (with §153):

| | trunkdb | SQLite | redb | sled |
|---|---:|---:|---:|---:|
| get by id | 2.0 µs | 16 µs | 2.0 µs | 2.3 µs |
| 1000 documents by index | 0.94 ms | 1.8 ms | 1.0 ms | 1.9 ms |
| count of 1000 by index | 0.01 ms | 0.03 ms | 0.04 ms | 0.26 ms |
| page 50 of 20 of a status | 26 µs | 54 µs | 45 µs | 252 µs |
| scan, 100,000 for a fifth | 58 ms | 43 ms | 54 ms | 67 ms |
| insert, 1000 per commit | 42k/s | 40k/s | 41k/s | 27k/s |
| update, 1000 per commit | 15k/s | 14k/s | 18k/s | 23k/s |
| delete, 1000 per commit | 31k/s | 19k/s | 25k/s | 36k/s |
| compact | 0.62 s | 0.20 s | 0.41 s | — |

Reads through an index are level with the others or ahead, and so are
batched inserts and updates in these runs; a full scan and compaction
are still behind. The full table, why each number
is what it is, and what each step changed are in
[DESIGN.md](DESIGN.md#12-performance).

## Documentation

- [GUIDE.md](GUIDE.md): how to use it, by example.
- [DESIGN.md](DESIGN.md): how it works today, one subsystem at a time.
- [spec/](spec/README.md): why, one numbered section per step (SPEC §N).
- [CHANGELOG.md](CHANGELOG.md): every step so far, by release.
- [ROADMAP.md](ROADMAP.md): what's left to settle the API and format, and what's open.

## Building

```
cargo build
cargo test
cargo doc --open
```

The minimum Rust version is 1.89, and CI builds with it. It rises
only when a dependency or a real need requires it, never for syntax
alone; a rise is noted in the changelog and isn't counted as breaking.

The current version is 0.31; 0.x versions may still change the API and
the file format, and each release says what breaks and how to move up.

## License

Licensed under the [MIT License](LICENSE).
