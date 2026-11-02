# The filter language

What `trunkdb find`, `fields` and `count` take after the collection,
and `aggregate`, `update` and `delete` as their filter argument. For
`find`, `fields` and `count` the words after the collection are joined
into one filter, but quote it anyway: strings, `>` and `*` mean
something to the shell.

## Contents
- A query
- Conditions
- Paths and names
- Values
- Strings, and a prefix
- Values of different types
- Null and missing
- Arrays
- Which conditions an index serves
- Mistakes

## A query

```text
[select <paths> | select *] [except <paths>] [where] [<condition>] [order by <path> [asc|desc], …] [limit <n>] [offset <n>] [use index <fields> | use no index]
```

Every part may be left out; nothing at all means every document.
`where` is needed only after a `select`, to say where the paths end.
`select * except body` hands out every field but `body`.
Keywords can be in any case. `_id` is always handed out, selected or
not; a selected field a document lacks is left out of its line.

`use index created` (or `use index (status, created)` for a compound
index, `use index _id` for the primary one) reads that index where the
planner would pick another; `use no index` scans. It changes how, never
which documents are found. A hint the index can't follow (no such
index, no condition on it) is an error, not ignored. Reach for it only
when `--explain` shows the planner reading far more entries than the
query returns.

## Conditions

| Text | Holds where |
|---|---|
| `a = 1`, `a != 1` (or `<>`), `<`, `<=`, `>`, `>=` | the comparison holds |
| `a in (1, 2, 3)`, `a not in (…)` | `a` is one of the list (or none of it) |
| `a contains "x"` | the string `a` has `x` in it, any case |
| `a is null`, `a is not null` | `a` is null or missing (or neither) |
| `exists(a)`, `not exists(a)` | `a` is present, even as null (or absent) |
| `size(a) > 2` | `a` is an array of more than two elements |
| `a has (b = 1 and c > 2)` | one element of the array `a` meets all of it |
| `not`, `and`, `or`, `( )` | as in SQL |
| `true`, `false` | always, never |

`not` binds tighter than `and`, and `and` tighter than `or`:
`a = 1 or b = 2 and c = 3` is `a = 1 or (b = 2 and c = 3)`.

Numbers compare across int and float: `age = 36.0` finds `36`. Values
of different kinds never compare equal, and a string is never less or
greater than a number: `age > 30` skips documents whose `age` is a
string. `order by` sorts across kinds as null and missing, booleans,
numbers, strings, ids (by when they were made), dates; arrays, objects
and binary come last, whichever the direction. Those three compare only
with `=` and `!=`, by their contents: `meta = {"k": 1}` finds an equal
object whatever its field order, and `<`, `>` never hold for them.

## Paths and names

- A path is names joined by `.`: `address.city`. A dot always
  separates; a key that has a `.` in its name can't be reached by any
  path (`trunkdb fields` marks it).
- A name is bare if it's letters, digits and `_`, not starting with a
  digit (`Größe` counts). Any other name goes in backticks, a backtick
  in it doubled: `` `first name` ``, `` `a``b` ``.
- These eleven words are reserved and need backticks as names: `and`,
  `or`, `not`, `true`, `false`, `null`, `select`, `where`, `order`,
  `limit`, `offset`. Other keywords (`size`, `date`, `is`, `has`, …) are
  fine as bare names: `size > 3` compares a field named `size`.

## Values

JSON, as a document holds it: `5` (int), `5.0` (float), `"text"` in
double quotes with JSON escapes (single quotes are an error), `true`,
`false`, `null`, arrays and objects.

- Ids: `id("0192f0c4-7d7a-7c43-9e0b-1f6d1b8f0a11")`, or the tagged
  `{"$id": "…"}`.
- Points in time: `date("2026-05-01T12:00:00Z")` (RFC 3339, always
  with a time, in UTC or with an offset: `date("2026-05-01")` is
  refused), or `{"$date": "…"}`. A date never equals a string: compare
  dates with `date(…)`.
- Binary: `{"$binary": "<base64>"}`; special floats: `{"$float":
  "NaN"}`.

## Strings, and a prefix

Strings compare by Unicode code point, so case counts: `"B" < "a"`,
uppercase before lowercase, and `"Ä"` after `"z"`. `order by` sorts
them the same way.

- `a contains "x"` finds `x` anywhere in `a`, in any case, but reads
  every document.
- There is no `starts with`, `like` or regex yet. For a prefix, use a
  range up to the prefix with its last character raised by one:
  `title >= "Fix" and title < "Fiy"`. It is exact, but case-sensitive,
  and an index on `title` serves it. For any case, `contains` and
  check the matches yourself.

## Values of different types

A comparison holds only between values of one kind: a string never
equals, or is less than, a number. So `hours > 3` passes over a
document whose `hours` is `"4"`, and `sum(hours)` and `avg(hours)`
leave it out. `trunkdb fields` shows which paths hold several types.

There is no test for a type yet. To find the documents where a field
holds something else than a number, or than a string:

```text
exists(hours) and not (hours >= 0 or hours < 0)     -- hours, not a number
exists(name) and not (name >= "")                   -- name, not a string
```

## Null and missing

| document | `a = null` / `a is null` | `exists(a)` | `not exists(a)` |
|---|---|---|---|
| `{"a": 1}` | false | true | false |
| `{"a": null}` | true | true | false |
| `{}` | true | false | true |

So `is null` finds both; `exists` and `not exists` tell them apart.
`a != 1` holds where `a` is missing too.

## Arrays

A path step ending in `[*]` goes on with every element of the array
there: `tags[*]`, `items[*].sku`, `grid[*][*]`. Without `[*]`, a path
doesn't look into arrays: `tags = "x"` compares the whole array with a
string, which never holds.

- A comparison on a `[*]` path holds if any element meets it:
  `tags[*] = "urgent"`, `scores[*] > 90`.
- `!=` is the exception: `tags[*] != "draft"` means no element is
  `"draft"` (and holds for a document without tags).
- Two conditions on the same `[*]` path can be met by different
  elements. For one element that meets several, use `has`: `items has
  (sku = "A1" and qty > 2)`. Inside `has`, paths start at the element,
  and `@` is the element itself: `scores has (@ >= 80 and @ < 90)`.

## Which conditions an index serves

An index on a field narrows the read for these conditions on it, ANDed
with anything else; the rest of the filter is checked on what it reads:

| Served by an index | Read every document |
|---|---|
| `=`, `<`, `<=`, `>`, `>=`, `is null` | `!=`, `not in (…)`, `not (…)` |
| `in (…)` | `contains` |
| an `or` whose every branch is served | an `or` with one branch that isn't |
| `has (…)`, `[*]` paths, with an index on that path | `exists(…)`, `size(…)` |

So write the values you want, `status in ("open", "late")`, rather
than the ones you don't, `status not in ("done", "cancelled")`.

An index on several fields, `(status, due)`, serves `=` on the first
and a range on the second together: `status = "open" and due < …`
reads only those entries. With `in` on the first field, each value's
entries are read whole, the range on `due` checked after; still fewer
than a scan. To read only what the range takes, write a branch per
value: `(status = "open" and due < X) or (status = "late" and due <
X)`. `find … --explain` shows which index a filter read, and
its `instead:` lines what the others would have read.

## Mistakes

A filter that doesn't parse exits with code 2 and prints what was
expected, the line, and a `^` under the place:

```text
trunkdb: filter: expected a value, found `Ada` at line 1 column 8
  name = Ada
         ^
```

The usual causes: a string without double quotes (`Ada` for `"Ada"`),
single quotes, a reserved word as a name without backticks (`order >
3` for `` `order` > 3 ``), a condition after `select` without `where`.
