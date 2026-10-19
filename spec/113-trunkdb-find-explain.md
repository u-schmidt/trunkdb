# 113. `trunkdb find --explain` (`src/bin/trunkdb.rs`)

`explain_run` (§94) said how a query ran, but only to Rust code. Now the
command says it too:

```text
$ trunkdb find app.trunkdb users 'age > 40' --explain
plan: index on age
read: age, where age > 40, 1 entry, its entries decide the filter
keys examined: 1
documents examined: 1
documents matched: 1
documents returned: 1
answered from index entries: 0
time: 0.05 ms
```

## 113.1 The option
`--explain` comes last, after the filter, as `--max-line` comes after
`import`'s arguments. A filter never is `--explain`, so the two can't be
mistaken for each other. The query runs as `find` would run it, through
`explain_run`, read-only; what it finds isn't printed, only how.
Mistakes are `find`'s (§112.3): a filter that doesn't read, a
collection that isn't there.

## 113.2 What it prints
A line for each thing `Explained` says, to standard output, so it can
be piped or kept beside another run's:
- **`plan:`** `QueryPlan` in words: `scan of every document`, `index on
  age`, `index on age, in its order` (`IndexOrder`), `indexes on email,
  age, together` (`IndexUnion`), `by id`. A plan a newer library adds
  (`QueryPlan` is `#[non_exhaustive]`) prints as its `Debug`.
- **`read:`** one line per index read, in the order they ran: the
  index, its bounds in the filter language (§111, the conditions'
  `Display`) or `the whole index`, how many entries, and `backward` and
  `its entries decide the filter` (§100) where they hold.
- **`sort:`** with a sort only: how many keys the index order gave, how
  many were left to memory.
- **The counts**, one per line: keys and documents examined, documents
  matched and returned, and how many were answered from index entries
  without their documents (§106.5).
- **`time:`** in milliseconds, last, since it alone differs from run to
  run (§94.4): every line but the last is what two runs can be compared
  on.

Nothing to standard error, where `find` writes its count: the counts
are on standard output already.

## 113.3 Rejected
- **JSON**, for scripts: the counts in `key: value` lines are as easy to
  pick out, and a program that wants more uses `explain_run` itself.
- **The plan without running it** (`explain`): the counts are what
  `explain_run` adds, and on a snapshot a run changes nothing.
- **`Display` for `Explained` in the library**: the words are the
  command's; a library user formats what they need from the fields,
  which are public.

## 113.4 Tests
`tests/cli.rs`, the command run as a process, every line but the time
compared:
- A range on an index whose entries decide it; no filter (a scan, the
  primary index read whole).
- An OR on two indexes, sorted by a field neither gives in order, with
  a limit: two reads, the sort in memory, one returned of two matched.
- A sort the index gives, read backward.
- A selection answered from a unique index's entry: one key examined,
  no document.
- A filter's mistake and a missing collection, with `--explain`.

## 113.5 Mutations
Each caught: `--explain` ignored (1 test); an index plan named a scan
(1); `in its order` left out (1); a union's indexes left out (1); the
bounds left out (1), or `the whole index` (1); `backward` (1) and `its
entries decide the filter` (1) never said; `1 entries` (1); the sort's
two numbers swapped (1), or its line left out (1); documents examined
(1) or returned (1) given as matched, keys as documents (1), and those
answered from entries as 0 (1); the time left out (1). The three counts
given as others survived at first: every case compared in full had
equal counts. The OR and the selection, compared in full since, have
counts that differ.
