---
name: trunkdb
description: Look into and change trunkdb database files with the `trunkdb` command — what collections they hold, what their documents look like, finding and counting documents, inserting, updating and deleting them, adding indexes, checking a file for damage, backing it up as JSON Lines. Use this whenever a task involves a trunkdb file (often `*.trunkdb`, with a `*.trunkdb.wal` beside it) or a database written by an app built on the trunkdb crate, even if the user just says "look at this database", "what's in this file" or asks a question about the data in it.
---

# Inspecting trunkdb files

trunkdb is an embedded document database: one file holds named
collections of schema-less JSON-like documents, each with an `_id`.
The `trunkdb` command reads such a file and answers in text and JSON
Lines, so you never need to read the file's bytes yourself: they are
pages of a B-tree, not text, and reading them only costs context.

Check the command is there with `trunkdb --help`. If it isn't, ask the
user before installing it (`cargo install --git
https://github.com/u-schmidt/trunkdb`, which needs Rust).

## Work from the outside in

A database can hold millions of documents, and everything a command
prints lands in your context. So learn the shape first, then ask
narrow questions:

1. **`trunkdb info <file>`** — the collections, how many documents
   each holds, and their indexes, and which trunkdb releases created
   and last wrote the file. A few lines; start here.
2. **`trunkdb fields <file> <collection>`** — every field path the
   documents have, in how many documents, and as which types. There is
   no schema, so this is the only way to know which fields exist
   without guessing from samples, and which are optional (count lower
   than the collection's) or mixed (several types). Paths are printed
   exactly as filters write them, so copy them. On a large collection
   add `limit 10000` to sum up a sample: it reads only those.
3. **`trunkdb find <file> <collection> '<filter>'`** — the documents a
   filter finds, one JSON document per line, then `found N documents`
   on stderr. Always give it `limit`, and `select` the fields you need:
   `'select name, team where status = "open" order by due limit 20'`.
4. **`trunkdb count <file> <collection> '<filter>'`** — how many
   documents a filter finds, the number alone on stdout; from index
   entries where they can tell, so it's cheap.
5. **`trunkdb aggregate <file> <collection> '<filter>' '<aggregate>'`**
   — counts, sums and the like per group, one JSON row per group, so
   only the totals reach you: `trunkdb aggregate app.trunkdb tasks
   'status = "open"' 'count() as n, sum(hours) as hours group by team
   order by n desc limit 10'`. `''` for no filter. The aggregate's
   language is in `references/aggregates.md`.
6. **`--explain`** after a filter (or an aggregate) says how it ran instead of what it
   found: the index used, entries and documents examined, the time. Use
   it when the user asks why a query is slow or whether an index helps.
   Its `instead:` lines are the plans it passed over, with the entries
   each would read; one marked `fewer` is the index to hint with
   `use index …`.
7. **`trunkdb get <file> <collection> <id>`** — one document by its id,
   written bare (`0192f0c4-…`) or as `id("…")`; exit code 1 if there is
   none.

`info`, `fields` and `check` take `--json` at the end: `info` and
`check` print one JSON object, `fields` one per path (`path`,
`documents`, `kinds`). Use it when you go on with `jq` or code; the
text is shorter to read.

The filter language is SQL's `WHERE` with a few additions for nested
documents and arrays. Wrap the whole filter in single quotes for the
shell; strings inside it take double quotes (`'name = "Ada"'`, never
`"name = 'Ada'"`), and a date is a full timestamp with a time and a
zone (`date("2026-10-01T00:00:00Z")`, not `date("2026-10-01")`). The
common forms:

```text
status = "open" and hours >= 4
name contains "ann"                      (substring, any case)
team in ("core", "web")
due is null                              (null or missing)
exists(address.city)                     (present, null included)
tags[*] = "urgent"                       (any element of the array)
items has (sku = "A1" and qty > 2)       (one element meets all of it)
size(tags) > 2
created >= date("2026-01-01T00:00:00Z")
owner = id("0192f0c4-…")
select name, address.city where age > 30 order by name desc limit 10 offset 20
```

For anything beyond these — backticks for names with spaces or that
are keywords, `@` inside `has`, how null and missing compare, what a
mistake message means — read `references/filters.md`.

## Reading what it prints

Documents are compact JSON with tags for what JSON has no type for:
`{"$id": "…"}` (a document id, a UUIDv7, so ids sort by creation
time), `{"$date": "…"}` (a UTC point in time), `{"$binary": "<base64>"}`,
`{"$float": "NaN"}`. In a filter, ids and dates are written `id("…")`
and `date("…")`. `5` is an int and `5.0` a float; they compare equal.

A filter that doesn't parse exits with code 2 and shows the line with a
`^` under the mistake: fix that spot and run it again. A misspelled
collection is an error (exit 1), not an empty result.

## What not to do

- **Don't `export` to answer a question.** `trunkdb export <file>
  [out.jsonl]` writes every document of every collection. It's the
  backup to make before anything changes the file, and the way to hand
  the data to another tool, but `fields` and `find` answer questions
  for a fraction of the output.
- **Don't write unless the user asked for that change.** `insert`,
  `update`, `delete`, `index`, `drop`, `compact` and `import` write to
  the file (see "Changing documents" below). `info`, `check`, `export`,
  `find`, `fields`, `count`, `get` and `aggregate` only read it and
  can't damage it.
- **"in use by another program"** means an app has the database open,
  which locks it: for a write, against every other open; for a read,
  against writes. Tell the user, and ask them to close the app. Don't
  copy the file to get around the lock, since a copy taken while the
  app writes can miss what is still in its `.wal` file.

## Changing documents

Each write is one atomic batch: it happens whole, or not at all. Work
like this:

1. **Count first.** Run the same filter with `count`, or the write with
   `--dry-run`, which says how many documents it would change and writes
   nothing. Tell the user the number if it isn't the one they expect.
2. **Back up before a large change:** `trunkdb export <file>
   backup.jsonl`. `import` of that into a new file undoes anything.
3. **Then write**, the filter and the update each one argument in
   single quotes:

```text
trunkdb insert app.trunkdb tasks '{"title": "Write docs", "status": "open"}'
printf '%s\n' '{"title": "a"}' '{"title": "b"}' | trunkdb insert app.trunkdb tasks -
trunkdb update app.trunkdb tasks 'status = "open" and due < date("2026-01-01T00:00:00Z")' \
  '{"$set": {"status": "late"}, "$inc": {"reminders": 1}}'
trunkdb update app.trunkdb counters 'name = "visits"' '{"$inc": {"n": 1}}' --upsert
trunkdb delete app.trunkdb tasks 'status = "done" and closed < date("2025-01-01T00:00:00Z")'
trunkdb index app.trunkdb tasks add status due      # one index on both, in that order
trunkdb index app.trunkdb users add email --unique
```

- **`insert`** takes one JSON object, or `-` and one per line on stdin;
  it prints each new id. Ids and dates are written `{"$id": "…"}` and
  `{"$date": "…"}` inside JSON. A collection that doesn't exist is an
  error, so a typo can't create one; add `--create` only when the user
  wants a new collection.
- **`update`** takes MongoDB's operators: `$set`, `$unset` (a list of
  paths), `$inc`, `$mul`, `$min`, `$max`, `$rename`, `$push` (with
  `{"$each": […]}` for several), `$addToSet` (with `$each`), `$pull`,
  `$pop` (1 the last, -1 the first), and `$setOnInsert` with
  `--upsert`. A whole document isn't replaced: `{"title": "x"}` without
  an operator is refused. `--upsert` updates the one match, or inserts
  a document made of the filter's `=` conditions and the update.
  "updated N documents" counts the documents that changed: one already
  as the update leaves it doesn't count.
- **An update or delete that would change every document** of a
  collection is refused unless you add `--all`. Add it only when the
  user asked for all of them.
- **`drop`** removes a collection with its documents and indexes; it
  can't be undone without a backup. Confirm with the user first.
- **`index add`** on a unique index fails, and adds nothing, if two
  documents already share a value; `find` them with the field sorted
  to show the user which.

## Checking a file

`trunkdb check <file>` reads the whole file and reports problems, exit
code 1 if there are any. Run it when the user suspects damage, or
before trusting a file that came from elsewhere. `info` also shows
where the pages go and how much `compact` would save.
