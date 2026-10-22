# The filter language

What `trunkdb find` and `trunkdb fields` take after the collection. The
words after the collection are joined into one filter, but quote it
anyway: strings, `>` and `*` mean something to the shell.

## Contents
- A query
- Conditions
- Paths and names
- Values
- Null and missing
- Arrays
- Mistakes

## A query

```text
[select <paths>] [where] [<condition>] [order by <path> [asc|desc], …] [limit <n>] [offset <n>]
```

Every part may be left out; nothing at all means every document.
`where` is needed only after a `select`, to say where the paths end.
Keywords can be in any case. `_id` is always handed out, selected or
not; a selected field a document lacks is left out of its line.

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
