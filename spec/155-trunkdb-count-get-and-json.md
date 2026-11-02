# 155. `trunkdb count`, `get`, and JSON for a program (`src/bin/trunkdb.rs`)

The `trunkdb` command is how an AI agent reaches a database without a
server of trunkdb's (§39, §112, §121, `skills/trunkdb/`): an agent has
a shell, and a command needs nothing running. It is also what a person
uses who doesn't install a graphical tool. Two questions it answered
only by a detour: "how many?", by printing every document found and
reading the count from stderr, and "this one", by a filter on `_id`.
And what it printed for a person, `info`, `fields` and `check`, a
program had to take apart. This is the first of three steps: reads
here, then writes (§156), then aggregates as text.

The file stays locked to one program that writes (§21.1, §88): an
agent works on it while no app has it open. A server inside the app
that holds it would lift that, and is not planned.

## 155.1 The commands
- **`trunkdb count <file> <collection> [filter]`:** the number the
  filter finds, alone on standard output, its skip and limit applied as
  `find` applies them. `Collection::count`, so from index entries where
  they can tell (§100, §101), reading no document. A missing collection
  is an error, a filter that doesn't read exit code 2, as for `find`.
- **`trunkdb get <file> <collection> <id>`:** the document, one JSON
  line as `find` prints it. The id is written bare, as `find` prints it
  inside `{"$id": …}`, or as a filter writes it, `id("…")`. None with
  that id: nothing on standard output, `no document <id> in
  <collection>` on stderr, exit code 1, as `grep` finds nothing. What
  isn't an id: exit code 2.
- **`--json` after `info`, `check` and `fields`:**
  - `info`: one object, the file's facts (`format`, `page_size`,
    `pages`, `free_pages`, `written_by`, `created_by`, `wal_bytes`),
    and `collections`, each with `name`, `documents`, `indexes` (each
    `fields`, `unique`, `sparse`) and `sizes` (bytes and pages, `null`
    where they can't be read).
  - `check`: one object, `ok`, `collections`, `documents`, `pages`,
    `problems`, and exit code 1 where it isn't ok, as the text has it.
  - `fields`: one object per path, in the text's order, `path`,
    `nameable`, `documents` and `kinds`, each kind's count, most first;
    the summary on stderr as before.

Every one opens the file read-only (§88).

## 155.2 Rejected
- **`--json` for `find`:** it prints JSON Lines already.
- **A count on `find`, `--count`:** a command of its own reads plainer,
  and an option that turns `find` into another command is one more
  combination to learn.
- **`get` taking several ids:** a filter does that, `_id in (id("…"),
  id("…"))`.
- **The ids of indexes in `info --json`, their names as `info` prints
  them:** a name like `(team, email)` would have to be taken apart
  again; `fields` is the list itself.
- **A parser crate for the arguments:** still a handful of commands,
  matched by hand (§39), and the library's users would compile it.

## 155.3 Tests
In `tests/cli.rs`, the command run as a process:
- `count_prints_how_many_a_filter_finds`: all, a filter, `limit`, an
  `offset` past the end, an empty collection, a missing one, a filter
  that doesn't read.
- `get_prints_the_document_with_an_id`: as `find` prints it, bare and
  as `id("…")`; none, not an id, a missing collection.
- `info_and_fields_print_json_for_a_program`: every collection with its
  count, indexes and sizes; `fields`' paths in the text's order, a
  filter, a mistake.
- `check_reports_a_damaged_page_and_fails`: the same problem as JSON,
  and the same exit code.
- `looking_into_a_file_opens_it_read_only`: every new command beside a
  read-only open of the file.
