# 159. A test of a value's kind (`query.rs`, `query_text.rs`)

`trunkdb fields` shows a path holding values of several kinds: `hours`,
numbers in most documents and strings in a few (§121). Finding those
few took a trick (§122.5): an agent wrote `exists(hours) and not (hours
>= 0 or hours < 0)`, the documents whose `hours` no number comparison
holds, which is right only because a string never compares with a
number (§34.1), and says nothing of what it wants. Data imported from JSON or written by an
older version of a program has such fields; the next step after seeing
them is to find them, and fix or skip them.

```rust
let odd = tasks.find(Filter::new().is_kind("hours", Kind::String))?;
```
```
hours is string
exists(hours) and hours is not number and hours is not null
```

## 159.1 What it tests
- **`Condition::IsKind { field, kind }`**, built by
  `Condition::is_kind(field, kind)` and `Filter::is_kind`: the field
  holds a value of `kind`.
- **`Kind`**: `Bool`, `Int`, `Float`, `Number`, `String`, `Binary`,
  `Array`, `Object`, `Id` and `Date`, named `bool`, `int`, `float`,
  `number`, `string`, `binary`, `array`, `object`, `id` and `date`, as
  `trunkdb fields` prints them (§121.2). `Number` is either kind of
  number, as a comparison takes `5` and `5.0` for one value; `Int` and
  `Float` tell them apart, where a program stores whole numbers as one
  and gets the other. NaN is a `Float`. `Kind::ALL`, `name()` and
  `of(&Document)` are public, for a program listing or checking them.
- **No null kind.** `is null` already finds null and missing fields
  alike (§32), and `x is null` keeps that meaning in the text; a null
  kind beside it would read the same and mean less. A stored null is
  `exists(x) and x is null`, as §45.1 has it.
- **A missing field is of no kind,** so it matches no `is`, and every
  `is not`, which is the NOT of an `is`, as for every comparison (§32).
- **The value itself, or its elements with `[*]`,** as for every
  condition: `tags is array` for an array whatever it holds, `tags[*]
  is string` where some element is a string. An empty array has no
  elements.
- **`Kind` is `#[non_exhaustive]`,** as `Op` is: a date without a time,
  when it comes (ROADMAP.md), is a kind added, not a change.

## 159.2 How it runs
In memory, on each document read, like `exists`: no index serves it,
and an entry never decides it, so another condition bounds the read
or it scans. An index's key tells a string, an id, a date and a bool by
its tag, but not an `Int` from a `Float` (§28), nor an array from its
elements in a multikey index (§42); a kind is a test to run on the
documents others found, not one to find them by. The paths it names
are read and no more (§99), as for any condition.

## 159.3 The text
`hours is string`, a kind after `is`, where `null` goes; `hours is not
string`, its NOT, as `is not null` is `is null`'s, and `Display` writes
a NOT of a kind so. Kinds are words only there, in any case, so a field
named `string`, `number` or `date` stays a field: `string is string`. A
word after `is` that's neither `null` nor a kind is an error that names
them all.

## 159.4 Rejected
- **A function,** `type(hours) = "string"`, as SQL's `typeof`: a
  function compared with a string invites `!=`, `<` and `in` on kind
  names, which mean little, and a misspelled kind would be a string
  that matches nothing instead of an error.
- **MongoDB's `$type`** with numbers or `"double"`: the kinds here are
  trunkdb's own, and already named in `trunkdb fields`.
- **A null kind** (159.1).
- **An array of a kind matching through its elements** without `[*]`,
  as MongoDB's `$type` does: everywhere else a path without `[*]` is the
  value itself (§42).
- **An index read for the kinds a key's tag tells** (159.2): a range
  per tag would serve `is string` and `is date`, not `is int`, and the
  case it is for, a few odd values among many, is served as well by an
  index on what else the query asks.

## 159.5 Tests
- `tests/kinds.rs`: `hours` of every kind, NaN, the empty string and
  array, null and missing, twice each, with indexes on `hours` and
  `team`. `each_kind_finds_the_values_of_that_kind`, every kind and its
  NOT against a table, through `find`, `count`, `apply`, the text read
  back and an index on another field;
  `an_array_is_an_array_and_its_elements_are_read_with_a_star`;
  `the_odd_values_of_a_field_are_found_as_an_agent_wants_them`, in a
  filter and an aggregate column's `filter`; `its_text_is_a_kind_after_is`,
  any case, fields named as kinds, `null`, the errors;
  `the_names_are_trunkdb_fields_names`.
- `IsKind` joins the conditions of the randomized tests checking the
  matcher against its model (`query.rs`, the model telling kinds apart
  on its own), the text read back (`query_text.rs`), and the `queries`
  fuzz target, against its model and in a `custom` closure.
- The full test suite, clippy, the MSRV build (1.89).

## 159.6 Mutations
Each run against the library's tests of `query` and `tests/kinds.rs`:

- `Kind::of`: an `Int` or a `Float` not a `Number`, an id a string,
  null of every kind: caught;
- `name`: `number` written `float`: caught;
- the matcher: a missing field read as null and null of every kind;
  every value of a `[*]` path of the kind rather than one: caught;
- the field not among those a read decodes (§99): caught;
- the text: `is not` read as `is`, or written so; the NOT not written
  as `is not`; a kind only in lower case; the kind's word not read
  past: caught.

One changes nothing and was left: an `ElemMatch`'s kind rewritten
without the array's path in front (`within`), which only index bounds
read, and a kind never bounds one.
