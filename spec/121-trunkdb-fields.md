# 121. `trunkdb fields` (`src/bin/trunkdb.rs`)

`info` (§39) names a file's collections and counts their documents;
`find` (§112) shows documents. Neither says what the documents look
like, and with no schema (§6) nothing else does: whoever meets a file,
a person or an AI working through a shell, has to read documents and
guess, and a field only some documents have is easily missed. Now the
command sums up a collection's shape:

```text
$ trunkdb fields app.trunkdb users
_id           3  id 3
address       3  object 2, string 1
address.city  2  string 2
age           3  null 1, int 1, float 1
`first name`  1  string 1
name          3  string 3
tags          3  array 3
tags[*]       2  string 2, object 1
tags[*].k     1  int 1
9 paths in 3 documents
```

## 121.1 The command
`trunkdb fields <file> <collection> [filter]`:
- **Read-only**, like `find` (§88), with `find`'s mistakes (§112.3):
  a filter that doesn't read exits with 2 and shows where; a collection
  that isn't there, or a missing file, exits with 1.
- **A filter sums up only what it finds.** `limit 1000` makes it a
  sample of a large collection; `status = "open"` the shape of one kind
  of document; `select` only some fields. The words after the
  collection are one filter, as for `find`.

## 121.2 What it prints
A line per path, to standard output: the path, the number of documents
with a value there, and those documents by the kind of value, most
first. How many paths and documents to standard error, as `find` keeps
its count there (§112.2).
- **Every count is of documents.** A document counts once for a path,
  and once for each kind it has there, however many values it has
  there: two string tags are one document under `tags[*]`, `string 1`.
  So every number on a line is out of the same documents, and the
  kinds add up to the first number or more, never less: more when a
  document has values of two kinds under an array.
- **Paths as a filter writes them** (§111.3), so a line pasted into
  `find` works: `address.city`, `tags[*]`, `tags[*].k`, `grid[*][*]`.
  The names are written by the filter language itself, through
  `Condition::exists`'s `Display`, so the two can't disagree: a name
  that isn't a bare word, or is reserved, in backticks, `` `first
  name` ``, `` `order` ``.
- **A name no path can name**, one with a `.` or a `[*]` in it (§31.2),
  is listed too, in backticks, with `(no path can name it)` after it,
  and so is everything below it. The documents have it; a filter can't
  reach it.
- **Null is a kind**, as stored: `age = null` is there, `null 1`; a
  missing `age` isn't, and isn't counted. The first number minus the
  documents read is how many lack the path.
- **Kinds by their names in the text**: `null`, `bool`, `int`, `float`,
  `string`, `binary`, `array`, `object`, `id`, `date`.
- **The order**: `_id` first, then names in byte order, each field
  before what's below it, an object's fields before its array's
  elements. The same file gives the same lines, so two runs can be
  compared with `diff`.
- **Aligned columns**, padded by characters, not bytes: `größe` lines
  up with `grid`.

## 121.3 How it counts
One pass over a cursor (§29), a document at a time: memory grows with
the paths, not the documents. The paths are a tree, a node per path,
its fields in a `BTreeMap` and its array's elements in a node of their
own, walked as the document is, so a known path costs no allocation.
Each node keeps the number of the last document it counted, overall
and per kind; that's what makes a document count once however many
elements reach the node.

## 121.4 Limits
- **At most 10,000 paths.** Documents whose names are data, an object
  keyed by id or by day, would grow the summary with every document.
  Past the limit a new path isn't counted, nor anything below it; the
  paths already kept still are, and standard error says `only the
  first 10000 paths found are counted; a filter can narrow it`. Which
  paths are kept depends on the order the documents come in, which a
  filter's `order by` sets.
- **The whole collection is read** without a filter: on a large one
  that takes as long as `export`, which is what `limit` is for.
- **No values.** What a path holds, `find … 'select path limit 5'`
  shows.

## 121.5 Rejected
- **A sample by default.** A summary of the first thousand documents
  could leave out a field only later ones have, and look complete; the
  whole collection is the true answer, and `limit` a choice.
- **Values on the line**: an example, or the smallest and largest. An
  example is a document's data in what is meant to be its shape, a
  minimum and maximum a scan's worth of comparisons per path, and both
  are a `find` or an aggregate (§107) away.
- **Counts of values instead of documents**: `tags[*] string 2` for
  two tags, beside `tags 1`, mixes two units on one line.
- **JSON output.** The lines are already one per path, with columns a
  script splits on two spaces; JSON would double their length for an
  AI that reads them.
- **Every collection at once.** `info` lists them; a summary per
  collection is what a question is about, and keeps the output to one.
- **A library API**, `Collection::fields()`. It would be API to keep
  through 1.x (§118) for one tool's use; the command needs only
  `Document` and a cursor, which are there.

## 121.6 Tests
`tests/cli.rs`, the command run as a process:
- Documents of every kind, nested objects, arrays of arrays, objects in
  arrays, a path that's an object in one document and an array in
  another, names in backticks, a reserved name, a name with a `.` and
  one below it, a name that sorts before `_id`, one of more bytes than
  letters: the whole output, line for line, and the count on stderr.
- A filter given as several words with `select` and `where`: only what
  it finds. An empty collection: nothing, `0 paths in 0 documents`.
- Every path that can be named, put in `exists(…)` and counted through
  the library, counts what its line says.
- A filter's mistake (exit 2, nothing on stdout, the `^`), a collection
  that isn't there, a missing file (not created), no collection
  (usage).
- 10,001 paths: 10,000 lines, those kept counted in both documents, the
  notice on stderr.
- Beside a program that holds the file read-only, `fields` reads it.

## 121.7 Mutations
Each caught, 19 in all: documents counted once per value, and kinds
counted once per value; `_id` not put first; an array's elements before
an object's fields (caught only after a path that is both was added);
a name no path can name not in backticks, and the mark not carried
below it; the path limit off by one, and its notice not printed; kinds
least first, and unsorted (caught after `address` got more objects
than strings); no padding, and padding by bytes (caught after `größe`
was added); the file opened for writing; the words joined without
spaces (caught after the test used `select name where`, which doesn't
read joined); the filter ignored; elements not walked, objects not
walked; the buffer not flushed.
