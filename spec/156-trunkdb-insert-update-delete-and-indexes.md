# 156. `trunkdb insert`, `update`, `delete`, and indexes (`src/bin/trunkdb.rs`)

The second of three steps (§155): an AI agent, or a person at a shell,
changes a database through the command, not only reads it. Before, the
skill told an agent to say that changing documents wasn't possible,
and to offer an export edited by hand, or Rust.

## 156.1 The commands
- **`trunkdb insert <file> <collection> <document | -> [--create]`:**
  one document as JSON, or `-`, one per line on standard input, empty
  lines skipped; all in one batch. Each new id printed, a line each,
  `inserted N documents` on stderr. JSON as an export writes it, so
  `{"$id": …}` and `{"$date": …}` are an id and a date. A line that
  isn't a JSON object stops it before anything is written, saying which
  line, exit code 2.
- **`trunkdb update <file> <collection> <filter> <update> [--all]
  [--upsert] [--dry-run]`:** `Collection::update_fields` with the
  update's operators (156.2); `updated N documents` counts those it
  changed, as `update_fields` does, so one the update leaves as it was
  doesn't count. `--upsert`: `upsert_fields` (§144), the one match
  updated or a document of the filter's `=`s and the update inserted,
  its id printed.
- **`trunkdb delete <file> <collection> <filter> [--all] [--dry-run]`:**
  `delete_many`.
- **`trunkdb index <file> <collection> add <field>… [--unique]
  [--sparse]`** and **`drop <field>…`:** one index on the fields in
  their order, over the documents there. One found there already is
  said, not an error; a unique one that finds two equal values fails
  and adds nothing (§33); dropping one that isn't there is exit code 1.
- **`trunkdb drop <file> <collection>`:** the collection, its documents
  and indexes, with how many documents went.

The filter is the text `find` takes (§111), but one argument, as the
update follows it: `''` for none. Each command is one batch, so it
happens whole or not at all: an operator that fails on the tenth
document leaves the first nine as they were. A write opens the file
for writing, so it is refused, "in use by another program", while
another program has it open, even only to read (§88). The file must
exist, as for `compact` (§39), but for `insert --create`, which makes
both the file and the collection.

## 156.2 An update as JSON
MongoDB's update document, which agents and people know:

```json
{"$set": {"status": "done"}, "$inc": {"tries": 1}, "$push": {"log": {"$each": ["a", "b"]}}}
```

- `$set`, `$setOnInsert`, `$inc`, `$mul`, `$min`, `$max`, `$pull`: an
  object of paths and values, each the `Update` method of that name.
- `$unset`: a list of paths, or an object whose values are ignored, as
  MongoDB writes it.
- `$rename`: paths to new paths, strings.
- `$push` and `$addToSet`: a value, or `{"$each": […]}`, `push_all` or
  one `add_to_set` per value.
- `$pop`: `1` for the last element, `-1` for the first, as MongoDB has
  them; `pop_last` and `pop_first`.
- The operators run in the order written, each path in its order.

A key without `$` is refused: it would mean replacing the whole
document, which MongoDB does with such an update, and an agent writing
`{"status": "done"}` would lose every other field. An unknown operator
is refused, naming the ones there are. Each refusal is exit code 2,
nothing written.

A text form of updates, beside the filter's, was the other way: one
more language to learn, where JSON is the one the documents are
written in.

## 156.3 Rails for a write
An agent writes what it was asked to, in a filter it wrote itself.
What it gets wrong most easily, it gets told before anything happens:
- **Every document:** an update or delete whose filter takes every
  document of the collection, more than one, is refused unless given
  `--all`, saying how many. Taken by counting, after the filter is
  read: `''`, `age > 0` where every age is, or `limit 5` of five are
  all caught alike. A collection of one document needs no `--all`.
- **A collection that isn't there:** `update`, `delete`, `index` and
  `drop` are an error on one, as `find` is (§112); `insert` too, unless
  given `--create`, so a misspelled name doesn't make a new collection
  and leave the documents where nobody looks.
- **`--dry-run`:** what `update` or `delete` would change, counted, and
  nothing written; for `--upsert`, whether it would insert or update.
  (Since §184.2 the write rehearsed as a batch's dry run, not counted.)

Rejected:
- **A question on the terminal, `y/n`:** an agent has no terminal, and
  a script none either.
- **`--yes` on every write:** an agent learns to add it to every call.
- **A backup taken by every write:** a copy of the whole file for each
  change; the skill tells the agent to `export` before a large one.

## 156.4 Tests
In `tests/cli.rs`, the command run as a process:
- `insert_adds_documents_and_prints_their_ids`: one, several from
  stdin with tags and an empty line, a bad line inserting none, a
  missing collection and file, `--create`, an unknown option.
- `update_changes_what_a_filter_finds_by_its_operators`: every
  operator and what it leaves; an update that changes nothing counted
  as none; one failing operator leaving the document as it was; eight
  kinds of update refused, each with its message.
- `update_and_delete_ask_for_all_and_can_run_dry`: three filters that
  take every document, `--dry-run`, one document needing no `--all`.
- `update_upserts_where_nothing_matches`: inserted, then updated, the
  same id, `$setOnInsert` once.
- `indexes_and_collections_are_added_and_dropped`: compound, unique,
  sparse, found there already, a unique one refused over two equal,
  dropped, a missing one, a missing collection; a collection dropped.
- `writes_refuse_a_file_in_use`: every write beside a read-only open.

## 156.5 Mutations
Each run against `tests/cli.rs`, all caught: `--all` asked for a
collection of one; every document taken without `--all`; `--dry-run`
writing, for `update` and for `delete`; `insert` making a collection,
or a file, unasked; `$pop`'s ends swapped; `$each` pushed as an
object; `$unset` of a list ignored; a field taken as `$set`; the
upsert's insert and update named the other way; a bad line not
stopping the insert; `--unique` not passed on; dropping a missing
index taken as done.
