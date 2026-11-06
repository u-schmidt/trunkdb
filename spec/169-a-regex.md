# 169. A regex (`pattern.rs`, `query.rs`, `query_text.rs`)

`contains` finds a substring in any case (§25), and `starts with` a
prefix (§158). Anything with a choice in it, "timeout" or "time-out",
four digits at the end, a word from a list, needed an OR of each
spelling, or a `Condition::custom` closure, which has no text, so a
search box, a config file and the command line couldn't say it. §25.4
left a pattern language open: `LIKE`'s `%` and `_`, or a regex. What a
`LIKE` pattern says, a regex says too, and a regex says much more, in
a syntax programmers know.

```rust
let timeouts: Filter = r#"notes matches "(?i)time-?out""#.parse()?;
let fixes = Filter::new().regex("title", Regex::new("^Fix(es)? ")?);
```

No file format changed.

## 169.1 `Regex` and `Condition::Regex`
- **`trunkdb::query::Regex`**, made by `Regex::new(pattern)` or
  `pattern.parse()`, is the pattern compiled, once, and shared through
  an `Arc`: a filter clones it for nothing, and every query and thread
  using it share the one compiled form. `as_str` gives the pattern back;
  two `Regex` are equal when their patterns are, as written.
- **`Condition::Regex { field, regex }`**, `#[non_exhaustive]`, made by
  `Condition::regex(field, regex)` and `Filter::regex(field, regex)`.
  It holds where the field is a string the regex matches somewhere in.
  Anything that isn't a string, a number, an array without `[*]`, null,
  a missing field, doesn't match; with `[*]`, an element that is a
  string it matches. `not` takes in everything else, as for `starts
  with`.
- **Syntax:** the `regex` crate's. It matches anywhere in the string,
  as `grep` does; `^` and `$` anchor it to the string's start and end,
  `(?i)` ignores case, `(?m)` makes them anchor to lines. There are no
  backreferences or look-arounds: without them, the crate matches in
  time linear in the string, whatever the pattern. A pattern from a
  user can't make a query hang, which a backtracking engine's
  `(a+)+$` would.
- **Why a type of its own,** not the pattern as a string in a
  `Compare`: compiling can fail, and the builder methods return `Self`,
  not a `Result`. With a `Regex`, a pattern that isn't one fails where
  it is made, as `Regex::new`'s `Err`, and the read never meets it.
  The filter holds it compiled, so a query compiles nothing.
- **Why `regex`, not `matches`:** `Filter::matches(&doc)` already
  says whether a document meets the filter (§35). The text says
  `matches`, which reads naturally there.

## 169.2 In the text
`field matches "pattern"`, `matches` a word after a field, as
`contains` is, so it takes no name from fields (§111.3): `matches
matches "a"` is the field `matches`. The pattern is a JSON string, its
backslashes doubled as JSON writes them, `code matches "^\\d{4}$"`,
and `Display` writes it so, reading back as the same filter. A value
that isn't a string is a mistake, at the value, and so is a pattern
that doesn't compile, with the crate's reason in one line.

The pattern can be a parameter (§163): `title matches :p`. When the
template is bound, the value must be a string that compiles, or `bind`
refuses it with `BindError::NotAPattern { name, offset, reason }`; as
for any parameter, it is never read as text, so a pattern with a quote
in it is a pattern. `trunkdb` takes it as `--param p=…`, as for any
filter.

## 169.3 Reading through an index
A regex that begins with `^` and fixed text, `^Fix`, can only match
strings that begin with that text. So the planner reads it as the
`starts with "Fix"` it implies (§158): the range of keys that begin
with the text, of an index on the field, alone or after a compound
index's `Eq`s, in an OR's union, inside `has`. Every document in the
range is then checked against the regex. `comparison` in `query.rs`
gives the planner this `starts with`, where it takes a field, an `Op`
and a value; nothing else in the planner changed.

The fixed text is found on the pattern's syntax tree, from
`regex-syntax`, the crate `regex` parses with: a sequence whose first
part is the start of the text (`^` or `\A`, not `(?m)^`, which is the
start of any line), then the literals right after it, until anything
else. `^Fix(es)?` gives `Fix`; `^ab*` gives `a`, as `b*` is a part of
its own; `^[F]ix` gives `Fix`, the crate making a class of one letter
that letter. An alternation, `^a|^b`, a group, `^(Fix)`, and a pattern
ignoring case give none, and read every document, as `contains` does.
Only what the tree says for certain is taken: a prefix too short reads
more than it needs; one too long would lose matches, so none is
guessed.

Index entries don't decide a regex: an entry's key may hold a string
cut short (§158.2), and even a whole one isn't run against the regex
there. Each document in the range is read and checked, so a count by
a regex reads its documents, where one by `starts with` may read none
(§158.2). Deciding a regex on entries whose key holds the whole string
is in ROADMAP.md, Deferred.

## 169.4 Limits
- **A pattern of at most 4,096 bytes,** and one whose compiled form
  stays under 1 MiB, a tenth of the crate's default: `\w{100}`, a
  thousand of any Unicode word character, passes it. Either is a
  `RegexError`, which says why in one line, where the crate's syntax
  errors are several, the pattern and a `^` under the place.
- **Memory:** a compiled regex keeps a cache for its lazy DFA, a few
  KiB to a few MiB as it is used, freed with the last clone.

## 169.5 Behind a feature
The `regex` feature, on by default, brings `regex` and `regex-syntax`,
the crates of the Rust project's own regex. It is not a public
dependency: `Regex` and `RegexError` are trunkdb's own, and nothing
of the crate's is in the API (ROADMAP.md, A settled API and format).
With `default-features = false`, trunkdb builds without both crates,
`Regex`, `Condition::Regex` and `Filter::regex` are not there, and
`matches` in a filter's text is a mistake that says it needs the
feature. CI lints and tests that build too.

On by default, because a filter's text is read from outside, a search
box, a config file, the command line, where nobody chooses features: a
program that has trunkdb should read every filter trunkdb writes.

## 169.6 Left out
- **`LIKE`**, `%` and `_`: a regex says the same, `^Fix.*` for
  `Fix%`, and a second pattern language would be a second syntax to
  learn and escape for.
- **A word index:** a search over long text in milliseconds is not a
  regex, which reads every document it can't bound, but an index of
  words, each word of a field an entry. Not until a program searches
  notes or messages so (ROADMAP.md, Deferred).
- **A backtracking engine,** for backreferences and look-arounds: a
  pattern could then take exponential time, and a filter's text comes
  from outside.
- **A regex on binary data:** the crate's `bytes` API would allow it,
  but a filter's text has no way to say bytes in a pattern that isn't
  confusing, and `starts with` takes a binary prefix already (§166).

## 169.7 Tests
In `pattern.rs`: the fixed prefix of eight patterns, and none for eleven;
errors in one line, no backreferences, no look-around, too big, too
long; matching anywhere unless anchored; equality by the pattern.

In `tests/regex.rs`:
- **Sixteen patterns,** against the `regex` crate's own answer for 600
  tasks of titles of every kind, long ones past a key's budget among
  them: `find` and `count` through the indexes, without one, in memory,
  from the text read back, and through the compound index with the team
  fixed; `[*]` on array elements.
- **The plan:** a fixed prefix reads the index, its keys the ones
  `starts with` reads, each a document checked; six patterns without
  one scan; the compound index's range; an OR as a union; `has` through
  an index on the elements.
- **Case and kinds:** case counting unless `(?i)`, numbers and missing
  fields matching nothing, `not` taking in the rest.
- **The text:** `matches` after a field, any case, a field named
  `matches`, backslashes and quotes written and read back, a pattern
  that doesn't compile marked at its string, a value that isn't a string.
- **A parameter,** bound, never read as text, refused with
  `NotAPattern` where it isn't a regex or a string.
- **Everywhere a filter goes:** a live query, an aggregate's column
  filter, a batch's precondition and `update_many`.
- **A sparse index** serves a fixed prefix, which rules out null.

Besides: the random filters of `collection.rs`, checked against a scan,
and those decided on index entries, take a regex one leaf in eight; the
fuzz program's leaves take one in thirteen, on every path; `trunkdb
count` and `find` with a regex and a parameter, and a pattern refused;
and a build without the feature refusing `matches`.

## 169.8 Mutations
Fifteen hand-made mutations, all caught:
- the planner not taking a regex's `starts with` (caught by the plan
  test and the sparse index's);
- the fixed text going on past a part that isn't a literal (the prefix
  test, and the random filters on entries and of `update_many`);
- any look at the start counting as `^`, `(?m)^` among them (the prefix
  test);
- `[*]` not looked through in the matcher (the sixteen patterns);
- the text writing the pattern unescaped (the text test, the sixteen
  patterns read back);
- a parameter's pattern not checked at bind (the parameter test);
- no limit on the compiled size, or on the length (the errors' test);
- the crate's error on several lines (the errors' test);
- an OR of regexes not bounded (the plan test);
- a compound index's range not taking a regex (the plan test);
- a regex not ruling out null for a sparse compound index (the sparse
  index's test);
- the regex's field not decoded (four of the random filter tests);
- `has` keeping the element's path in the regex (the plan test);
- numbers matched by their digits (the sixteen patterns, the kinds'
  test).

The first found that `bounds_for` and `comparison` each decided on
their own whether a regex bounds an index, and that a disagreement
between them recursed without end; `bounds_for` asks `comparison` now.

## 169.9 Found on the way: a sparse index inside an AND or an OR
With regex leaves in it, the fuzz program made other programs, and one
found a filter and its own text planning apart: the filter had an OR
of one branch, `b starts with "s3"`, which the text writes as the
branch alone. A sparse compound index on `(a, b)` was offered to the
text, read back flat, and not to the filter, so a hint to use it was
followed by one and refused by the other. `excludes_null`, which says
whether the conditions keep out the documents a sparse index has no
entry for (§44), looked only at the filter's top-level comparisons. It
looks inside an AND now, whose conditions all hold, and an OR whose
every branch rules null out; an empty OR holds for nothing, so it
does too. The same was true at 0.32.0, without the regex. Answers
never differed: only which index was read.
