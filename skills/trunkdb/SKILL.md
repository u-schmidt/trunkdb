---
name: trunkdb
description: Look into trunkdb database files with the `trunkdb` command — what collections they hold, what their documents look like, finding and counting documents, checking a file for damage, backing it up as JSON Lines. Use this whenever a task involves a trunkdb file (often `*.trunkdb`, with a `*.trunkdb.wal` beside it) or a database written by an app built on the trunkdb crate, even if the user just says "look at this database", "what's in this file" or asks a question about the data in it.
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
4. **Counting** without printing: send the documents away and keep the
   count, `trunkdb find app.trunkdb tasks 'status = "open"' >
   /dev/null` prints only `found 312 documents`. For counts per group,
   select the one field and count in the pipe, so only the counts reach
   you: `trunkdb find app.trunkdb tasks 'select team where status =
   "open"' 2>/dev/null | jq -r .team | sort | uniq -c`. The count line
   goes to stderr; keep it out of `jq` (`2>/dev/null`, not `2>&1`).
5. **`--explain`** after a filter says how it ran instead of what it
   found: the index used, entries and documents examined, the time. Use
   it when the user asks why a query is slow or whether an index helps.
   Its `instead:` lines are the plans it passed over, with the entries
   each would read; one marked `fewer` is the index to hint with
   `use index …`.

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
- **Don't run `compact` or `import` unless the user asks.** They write
  to the file. `info`, `check`, `export`, `find` and `fields` only read
  it and can't damage it, even beside a running app.
- **"in use by another program"** means an app has the database open
  for writing, which locks it. Tell the user; don't copy the file to
  get around the lock, since a copy taken while the app writes can miss
  what is still in its `.wal` file.
- **Changing documents isn't possible through the command yet.** Say
  so, and offer what is: an export, edited and imported into a new
  file, or a few lines of Rust with the trunkdb crate.

## Checking a file

`trunkdb check <file>` reads the whole file and reports problems, exit
code 1 if there are any. Run it when the user suspects damage, or
before trusting a file that came from elsewhere. `info` also shows
where the pages go and how much `compact` would save.
