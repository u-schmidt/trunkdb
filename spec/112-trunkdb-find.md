# 112. `trunkdb find` (`src/bin/trunkdb.rs`)

The command could look into a file (§39) but not into its documents,
short of exporting all of them. A filter has a text since §111, so now
it can:

```text
$ trunkdb find app.trunkdb users 'age >= 30 order by name limit 2'
{"_id":{"$id":"0192…"},"name":"Ada","age":36}
{"_id":{"$id":"0192…"},"name":"Bob","age":41}
found 2 documents
$ trunkdb find app.trunkdb users select name where 'tags has (@ = "b")' | jq -r .name
Ada
```

## 112.1 The command
`trunkdb find <file> <collection> [filter]`:
- **Read-only**, like `info`, `check` and `export` (§88): it changes
  nothing in the file or its WAL, and runs beside a program that has
  the file open read-only.
- **Without a filter**, every document of the collection.
- **The words after the collection are one filter**, joined by spaces:
  `find app.trunkdb users limit 5` needs no quotes. A filter with
  strings or `>` needs them for the shell anyway, so one quoted
  argument is what the usage shows.

## 112.2 What it prints
- **One document per line**, as `Display` writes it (§71): compact
  tagged JSON, `_id` first, ids, date-times and binary as their tags.
  A line reads back as the stored document, `parse::<Document>()`.
  It is not an export: no header line, no `$collection`. Lines go to
  `jq` and the like.
- **How many** to standard error, `found 2 documents`, so standard
  output is only documents, as `export` keeps it (§39.1).
- **Streamed** from a cursor (§29) through a buffer: memory doesn't
  grow with what it finds, and the first lines come before the last
  are read.
- **A reader gone** — `| head` once it has its lines — ends it quietly,
  with exit code 0: what anyone wanted was printed. Rust ignores
  `SIGPIPE`, so a write fails with `BrokenPipe` instead of the process
  being killed; any other failed write is an error.

## 112.3 Mistakes
- **A filter that doesn't read** exits with 2, a usage error (§39.1),
  printing nothing on standard output. The message is §111.5's, with
  the line of the filter it's on and a `^` under the place:
  ```text
  trunkdb: filter: expected a value, found `Ada` at line 2 column 10
      name = Ada
             ^
  ```
- **A collection that isn't there** is an error, exit code 1: a typo
  would otherwise find nothing, as if nothing matched. An empty
  collection finds nothing, and says so.
- **A missing file** is an error and isn't created, as for the other
  commands that read.

## 112.4 Rejected
- **An export of the matches**, header and all, to import elsewhere.
  `export` is the whole database; a filtered export is a case of its
  own, not this one.
- **Pretty-printed or table output:** one compact line per document is
  what tools read; `jq` indents. A table needs columns, which documents
  don't have.
- **Exit code 1 when nothing is found**, as `grep` does: a script that
  counts matches would have to tell it from an error.
- **`--explain`**, the plan instead of the documents: left for a step
  of its own, which §113 is.

## 112.5 Tests
`tests/cli.rs`, the command run as a process:
- Every document, then a filter, then one given as several words with
  `select` and `order by desc`: the lines, their order and the count on
  stderr. Each line read back equals the stored document. An empty
  collection: nothing, `found 0 documents`.
- A filter's mistake on the middle one of three lines: exit 2, nothing
  on stdout, the message, that line alone and the `^` under the place.
  A collection that isn't there, a missing file (not created), and
  `find` without a collection (usage).
- 20,000 documents read by a reader that stops after 100 bytes: exit 0,
  nothing on stderr.
- Beside a program that holds the file read-only, `find` reads it
  (§88).

## 112.6 Mutations
Each caught: a closed pipe reported as an error (1 test); the
collection not checked (1); the words joined without spaces (1); a
filter's mistake with exit code 1 (1); the `^` counted from the start
of the filter, not of its line (1), and the line not cut at its end (1,
after the mistake was moved off the filter's last line: no test had one
there); the count on stdout (2); lines without a newline (1); the count
not counted (2); the file opened for writing (2); the buffer not
flushed (1).
