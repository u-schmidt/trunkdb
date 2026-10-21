# 111. Filters as text (`query_text.rs`)

A filter could only be built in Rust, with `Filter`'s methods. A tool
that takes queries from a person — a command line, a tool's text box,
a query kept in a config file — had to make up its own syntax and
its own translation. Now a `Filter` has a text form, both ways, as a
`Document` has JSON (§71):

```rust
use trunkdb::query::Filter;

let filter: Filter = r#"
    status = "Complete" and seen >= 10
    order by started_at desc
    limit 1
"#.parse()?;
let newest = tasks.find(filter)?;

let page = Filter::new().eq("team", "core").sort_asc("name").skip(40).limit(20);
assert_eq!(page.to_string(), r#"team = "core" order by name limit 20 offset 40"#);
```

The text says what the builders say, no more. It compiles to the same
`Filter`, which the planner, `explain` and every read see as they see
one built in code: nothing behind the parser changes.

## 111.1 A query
```
[select <paths>] [where] [<condition>] [order by <path> [asc|desc], …] [limit <n>] [offset <n>]
```
Every part may be left out; the empty text is `Filter::new()`.
- **`select`** is `Filter::select` (§106). After it, `where` is needed
  before a condition, to say where the paths end.
- **`where`** may be left out without a `select`: a text box for a
  condition reads best without it.
- A condition that is an `and` of several is several conditions, as
  `Filter::and` adds them.
- **`order by`** is `sort_asc`, `then_desc` and the like (§47); `asc`
  may be left out.
- **`limit`** and **`offset`** are `limit` and `skip` (§72), in that
  order, as SQL has them.

Keywords are read in any case: `ORDER BY` is `order by`.

## 111.2 Conditions
| Text | `Condition` |
|---|---|
| `a = 1`, `!=` (or `<>`), `<`, `<=`, `>`, `>=` | `eq`, `ne`, `lt`, `lte`, `gt`, `gte` |
| `a contains "x"` | `contains` (§25) |
| `a is null`, `a is not null` | `is_null`, `is_not_null` (§32) |
| `exists(a)`, `not exists(a)` | `exists`, `missing` (§45.1) |
| `size(a) > 2` | `size` (§45.2) |
| `items has (sku = "A1" and qty > 2)` | `elem_match` (§46) |
| `not`, `and`, `or`, `( )` | `Not`, `All`, `Any` (§36) |
| `true`, `false` | `all([])`, `any([])` |

`not` binds tighter than `and`, and `and` tighter than `or`, as in SQL:
`a = 1 or b = 2 and c = 3` is `a = 1 or (b = 2 and c = 3)`. A
comparison is one unit: `not a = 1` is `not (a = 1)`.

**`=` is trunkdb's `eq`, not SQL's.** `a = null` holds where `a` is
null, as `eq("a", Document::Null)` does (§32), where SQL's is never
true. `is null` says the same and is how the text prints it.

Inside `has`, paths start at the element (§46), and **`@`** is the
element itself, `""` in the builder: `scores has (@ >= 80 and @ < 90)`.
`@` is JSONPath's and LiteDB's name for the current element. It stands
alone: a field of the element is named without it.

## 111.3 Paths
A path is as a builder takes it (§31, §42): names joined by `.`, each
followed by any number of `[*]` — `address.city`, `items[*].sku`. No
space inside.

A name is bare if it is a word, letters, digits and `_`, not starting
with a digit. Any other name is written in **backticks**, a backtick in
it doubled: `` `first name` ``, `` `a``b` ``, ``` `` ``` for the empty
one. Letters are Unicode's: `Größe` is bare.

**Reserved** are only `and`, `or`, `not`, `true`, `false`, `null`,
`select`, `where`, `order`, `limit` and `offset`: a field of one of
those names is written in backticks, `` `order` ``. The other keywords
mean something only where a field can't be, so they stay free for
fields: `size > 3`, `date = …` and `is = 1` compare fields of those
names, and only `size(` and `exists(` are functions.

**Kept free.** A word added later comes where a field can't be: after
a field, where operators go (`in`, §115; `between`, `starts with`);
before `(`, as a function (`count()`); after a function (`as`); or
followed by what can't follow a field (`group by`, `having`). So none
needs reserving now. What needs keeping are three places, each an
error today, so that nothing claims them first:
- **A bare word as a value.** `a = b` is refused, `expected a value`.
  If a filter ever compares a field with a field (§111.8), that's how
  it reads.
- **`:` and `$` outside strings.** `:` is for parameters,
  `status = :status` (§111.8); `$` for references.
- **The start of another kind of text.** Updates and aggregates as text
  (§111.11) get texts of their own, read by their own parsers, so a
  word like `set` that starts an update never meets a field named
  `set` in a filter.

## 111.4 Values
A value is JSON as a `Document` reads it (§71): `5` is an `Int`, `5.0`
a `Float`, strings in double quotes with JSON's escapes, `true`,
`false`, `null`, arrays, objects, and the tags — `{"$binary": …}`,
`{"$float": "NaN"}`. Since the value is a `Document`, its type is kept
as exactly as a typed field in a form would keep it: a round trip never
turns a date into a string or `5.0` into `5`.

Ids and date-times have short forms, `id("0192…")` and
`date("2024-05-01T12:00:00Z")`, which is how they print; `{"$id": …}`
and `{"$date": …}` read as the same values.

Single quotes aren't strings: `'x'` is an error that points at the
`'`. Two kinds of string, with two kinds of escapes, would be one more
thing to know, for a language whose values are JSON.

## 111.5 Errors
`ParseFilterError` says what was expected and what was found, with a
line and column, the way `ParseDocumentError` does:

```
expected a value, found the end at line 1 column 5
expected a field, found the keyword `order`; a field of that name is written `` `order` `` at line 2 column 7
not an id: … at line 1 column 11
```

`offset()` gives the place in bytes, for a tool to mark it in the text.

## 111.6 Printing
`Display` for `Filter` and `Condition` writes the text, which reads
back as the same query:
- `select` only if there is one; `select([])`, `_id` alone, as
  `select _id`, which selects the same (§106.2).
- `where` only after a `select`.
- Parentheses only where they are needed; `asc` never; `offset` only
  if not 0.
- Names in backticks only where they must be.
- `eq` and `ne` with null as `is null` and `is not null`.

Printing normalizes, so a text read and printed may differ from the
text read: `a <> 1` prints as `a != 1`, `{"$id": …}` as `id(…)`, and an
`and` or `or` of one condition as the condition. Read again, the
printed text prints the same. `explain` doesn't print it yet; it could.

A `Condition` prints as it reads after `where`, so a tool that keeps
conditions apart, a list of them, prints each on its own.

## 111.7 What a form can take
A tool with a form of conditions — a list, all to hold, each a field,
an operator and a typed value, plus a sort field — can put its query
into text always: each row a condition, joined by `and`. Text goes back
into such a form only where it has that shape: the filter's conditions
each a comparison, `exists` or `not exists`, and as many sort keys as
the form has. Anything else, an `or`, a `has`, stays text. The tool
decides that from the `Filter`'s text or from its own; nothing here is
for it alone.

## 111.8 Rejected
- **LINQ-like** (`where … orderby … descending take 1`) or
  **Mongo-like** (`{"seen": {"$gte": 10}}`) syntax. The builder
  already reads like LINQ's method form, and Mongo's is tiring to type.
  SQL's is what most people who type queries know. Another front end
  can compile to the same `Filter` later.
- **SQL's `=` for null** — never true — and three-valued logic: the
  text means what the builder means (§32), or it would be a second
  language with the same words.
- **`LIKE`** for `contains`: it promises patterns, which trunkdb doesn't
  have (§25.4).
- **Single-quoted strings:** §111.4.
- **A leading `where` required:** noise in a text box that takes a
  condition only.
- **Parameters,** values passed apart from the text: open, for when
  code holds queries as text with gaps (a query in a config file,
  bindings for other languages). Until then, a value spliced into a
  query should be printed by its `Document`'s `Display`, or the filter
  built in code. They would be named, `status = :status`, as SQLite
  and Oracle have them: a `:` never starts a value, and names read
  better than counting `?`s. Not `$1`: `$` already marks JSON's type
  tags (§71), and is kept for references and operators, as in
  `{"_id": {"$eq": 17}}`. Not `@0`, LiteDB's: `@` is the element inside
  `has`.
- **Expressions** (`a + b > 3`, field compared with field): a filter
  can't say them (§107.8).

## 111.9 Tests
`query_text.rs`:
- The examples read as the filters their builders make, `select`,
  `has`, `size`, `is null`, `<>`, arrays and objects among them.
- Ids and date-times in their short forms and as tags, the tags
  printed as the short forms.
- `not` before `and` before `or`, and parentheses printed only where
  needed.
- Keywords in any case; keywords that aren't reserved as field names;
  names in backticks, with a backtick, empty, a keyword, a dot, `@`.
- Each kind of mistake, with its message and place.
- The places kept free (§111.3) refused: a bare word as a value, `:`
  and `$`, `group by`, `set`.
- A filter read from text finds what its builder finds, sorted, paged
  and selected.
- 3,000 random filters, every kind of condition nested three deep,
  values of every type (NaN, infinity, `i64::MIN`, strings with quotes
  and newlines, binary, ids, date-times, arrays, objects), names that
  need backticks: each one's text reads back as a filter that prints
  the same text and picks the same documents from 40 random ones, in
  the same order.

Fuzzing:
- The `queries` target (§105) reads each query's typed answers through
  the filter its text reads back as, held against the model like the
  others, and checks that the text prints the same.
- A new target, `filter_text`: any text as a filter. Reading never
  panics, and a filter read prints as text that reads back and prints
  the same.

## 111.10 Mutations
Each caught: `not` binding looser than `and` (2 tests); `<>` read as
`=` (1); `<=` read as `<` and `=` (2); an `or` inside an `and` printed
without parentheses (2), and a `not`'s condition (1); a lone condition
read as several (1); `is not null` read as `is null` (3); a reserved
name printed bare (2); a backtick in a name not doubled when printed
(2), and not undoubled when read (2); an id printed as its tag (1); a
string's escapes ignored (1); strings inside an array or object
ignored where its brackets are counted (2, after a string with `]` and
`}` was added to the tests: none had one); `offset` printed as `limit`
(2), and printed when 0 (2); `where` not required after a `select`
(1); `size` and `exists` functions without their `(` (1); columns
counted from 0 (1); a lone condition kept in an `and` of one (4);
keywords read in lower case only (1); `desc` read as `asc` (4); a
condition printed in parentheses (1); `not` read as nothing (4); `@`
read as a field named `@` (3); the empty path printed as backticks
(1); `size`'s comparison ignored (2).

Two change nothing anyone could see: an `or` inside an `or` printed
without parentheses — `a or (b or c)` as `a or b or c` — and the
condition inside `has` in parentheses of its own.

## 111.11 Limits
- No parameters (§111.8).
- Aggregates have no text yet: `group by … count() as n` would be a
  text of its own, with this one's conditions inside (§111.3). Its
  functions, `min(` and the like, are functions only before `(`.
- Updates neither; theirs too would be its own, so `set`, `push` or
  `pull` never mean anything in a filter.
- On the command line through `trunkdb find` since §112.
