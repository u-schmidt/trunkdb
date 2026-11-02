# 158. `starts with` (`query.rs`, `index/key.rs`, `query_text.rs`)

An agent asked for the tasks whose title starts with "Fix" wrote the
range by hand, `title >= "Fix" and title < "Fiy"` (§122.5): right, but
a trick to know, and one that goes wrong at a prefix ending in the
last character there is. A prefix is the common case of a pattern,
"names beginning with", "paths under this folder", "codes of this
series", and the one case an index serves as it is. This adds it; the
pattern language around it stays open (ROADMAP.md).

```rust
let fixes = tasks.find(Filter::new().starts_with("title", "Fix"))?;
```
```
title starts with "Fix"
```

## 158.1 What it matches
- **`Op::StartsWith`**, built by `Filter::starts_with(field, prefix)` and
  `Condition::starts_with`: the field is a string, and its bytes begin
  with the prefix's bytes, as Rust's `str::starts_with` has it.
- **Case counts,** as it does for `<` and `=`: "Fix login" starts with
  "Fix", not with "fix". `contains` ignores case (§25), but a prefix
  that ignored it couldn't be read from an index, whose keys order
  strings by their bytes; a search box that wants both writes `contains`
  or an `or` of the spellings it expects.
- **Every string starts with `""`,** so `starts with ""` is "is a
  string".
- **Anything that isn't a string** doesn't match, on either side, as for
  `contains`: a number, null, a missing field, an array (its elements
  with `[*]`, as for every comparison), and a prefix that isn't a string,
  which the builders can't give but `Condition::compare` can.
- **`not`** takes in all the rest, missing fields too, as for every
  comparison (§32).

## 158.2 From an index
A string's key is its bytes, a `0x00` written `0x00 0xFF`, cut where the
key's budget ends (1,005 bytes for an index on one field, 123 for one on
eight), then `0x00 0x00` (§28.1, §43). So:

- **The range** is the keys that begin with the prefix's escaped bytes,
  `KeyRange::prefixed` of the encoding without its terminator. A string
  beginning with the prefix has such a key, cut or not; where the
  prefix is longer than the budget, its cut form is the range, and the
  documents decide the rest. The planner takes it as it takes any
  range: on its own, after the `=`s fixing a compound index's first
  fields, intersected with other ranges on the field, and in `explain`
  as `where title starts with "Fix"`.
- **An entry decides it** (§100, §104): a key beginning with the
  escaped prefix holds a string that begins with the prefix, since an
  escaped prefix never runs into the terminator (a `0x00` in it is
  followed by `0xFF`, the terminator's by `0x00`). A key that doesn't
  holds one that doesn't, unless it was cut before the prefix ended:
  undecided then, and the document is read. So a count, or a read of
  ids, reads no document, long strings included where the prefix ends
  within the budget.
- **`_id`** is never a string: `starts with` on it matches nothing.

## 158.3 The text
`title starts with "Fix"`, two words after a field, where operators go:
§111.3 kept the place for it. Neither word is reserved: `starts = 1`
compares a field named `starts`, and `starts starts with "a"` is a
prefix of it. `starts` without `with` is no operator, and the error
points at `starts`. `Display` writes it back the same way.

## 158.4 Rejected
- **A range written by the parser,** `starts with "Fix"` read as `>=
  "Fix" and < "Fiy"`: the next string after every string with a prefix
  isn't always a string (a prefix ending in `U+10FFFF`), and the text
  wouldn't read back as written.
- **Ignoring case,** as `contains` does: no index serves it (158.1).
- **`like` or a regex now:** the pattern language is a decision of its
  own (§25.4). `starts with` is what an index serves of it; a pattern
  that begins with a fixed prefix can later read the same range.
- **A prefix on `_id`,** of an id's text: ids compare by their bytes,
  not their text.

## 158.5 Tests
- `a_prefix_range_holds_the_strings_that_begin_with_it` (`index/key.rs`):
  strings with `0x00`, `0x01` and non-ASCII bytes, and of lengths around
  both budgets, each against each as a prefix, in keys of one field and
  of eight: the range holds every string that begins with the prefix,
  and only those where neither is cut; an exact key is always decided,
  as `str::starts_with` says; a cut one beginning with the whole prefix
  too; values of other types never.
- `tests/starts_with.rs`: 600 tasks with titles in both cases, the
  empty one, one with a `0x00`, past both budgets, numbers, null,
  missing and arrays, indexes on `title` and `(team, title)`.
  `it_finds_what_a_scan_finds_through_an_index_and_without`, eleven
  prefixes against a model, through `find`, `count`, a scan, `apply`,
  the compound index and the text read back; `[*]`;
  `case_counts_and_only_strings_match`;
  `an_index_reads_only_the_keys_that_begin_with_the_prefix`, as many
  keys read as found; `the_entries_decide_it_without_reading_a_document`;
  `its_text_is_two_words_after_a_field`.
- `StartsWith` joins the operators of the randomized tests that check
  index reads, entries deciding and covered reads against a scan
  (`collection.rs`), the matcher against its model (`query.rs`), the
  text read back (`query_text.rs`), and the `queries` fuzz target; the
  random strings gain `"x"`, a prefix of the long ones.
- The full test suite, clippy, the MSRV build (1.89).

## 158.6 Mutations
Each run against the library's tests of `index::key` and `query`, and
`tests/starts_with.rs`:

- an entry: a mismatch never decided false, or always, even where the
  key was cut before the prefix ended; a match left undecided; a `0x00`
  in the prefix compared unescaped; a value of another type left
  undecided; a prefix that isn't a string left undecided (caught once
  a test counted the documents read): caught;
- the range: none, so a scan; the terminator kept, so only the prefix
  itself; one byte short: caught;
- the match ignoring case, or a `contains`: caught;
- the text: `starts` not given back when `with` doesn't follow; `with`
  optional; written without `with`: caught.

One changes nothing and was left: `starts with` on `_id` decided by
`holds`, which is false for it anyway.
