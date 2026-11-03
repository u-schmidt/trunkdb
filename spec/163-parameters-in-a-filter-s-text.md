# 163. Parameters in a filter's text (`query_text.rs`, `bin/trunkdb.rs`)

A program that takes a value from its user and puts it into a filter's
text builds the text with `format!`:

```rust
let filter: Filter = format!(r#"name = "{name}""#).parse()?;
```

A name with a `"` in it breaks the text, and a name like
`x" or true or name = "y` changes what the filter means: SQL injection,
in miniature. The builder (§35) never had this problem, but a text kept
in a file, a setting or a text box can't be a builder. On the command
line, the same value is quoted twice, once for the shell and once for
the text: `'name = "O'\''Brien"'`.

A parameter is a name where a value stands, `:name`, and its value is
passed apart from the text, as `@name` and a `SqlParameter` are in
ADO.NET, or `?` in SQLite. The value is never read as text, so nothing
in it can change the filter. §111.3 has refused `:` outside strings
since the text language began, keeping it for this. No file format
changed, and no text that reads today reads differently.

```rust
let open: Template<Filter> = "status = :status and owner = :me order by due limit :n".parse()?;
let filter = open.bind(&Params::new().set("status", "open").set("me", user).set("n", 20))?;
```

```
trunkdb find app.trunkdb tasks 'status = :status limit :n' --param status=open --param n=20
```

## 163.1 Where a parameter stands
- **`:name` where a value stands:** after a comparison (`=`, `!=`, `<`,
  `<=`, `>`, `>=`, `contains`, `starts with`), as a value in an `in`
  list (`status in ("open", :other)`), and in an aggregate's `having`
  and in a column's `filter (where …)`, which are conditions too.
- **`in :list`, the whole list:** a parameter in place of the
  parenthesized list. Its value must be an array, and each element is a
  value of the list, as `Condition::is_in` takes them. An empty array is
  `in ()`, which nothing matches.
- **Where a count stands:** `limit :n`, `offset :n`, and
  `size(tags) > :n`. The value must be a whole number from 0, as the
  literal must be.
- **Nowhere else:** not a field, a sort key, an operator, an index in a
  hint, a selected path, an aggregate's column or name, a window's
  duration, or the string inside `id(…)` and `date(…)`. A text that
  needs a field chosen at run time is a filter built in code. A typed
  value needs no `id(…)` around the parameter: the value passed for
  `:id` is a `Document::Id` already, and `_id = :id` compares with it.
- **A name** starts with a letter or `_` and goes on with letters,
  digits and `_`, as a bare field name does; `:` and the name have no
  space between them. Names are case-sensitive. The same name may stand
  in several places, and takes the same value in each.

## 163.2 What a bound parameter means
Binding a value means what writing that value in the text would mean,
and nothing else. `status = :s` with `"open"` is `status = "open"`;
with `null` it is `status = null`, which in trunkdb means the field is
null or missing (§32.1), as in the text and in the builder. With `5`
it compares as `5` compares, an `Int`; with the string `"5"`, as the
string. No value is turned into another type to fit: the value passed
is the `Document` compared, with its type, as `Filter::eq` takes it.

## 163.3 The API
- **`Template<T>`,** for `T` one of `Filter`, `Condition` and
  `Aggregate`, read with `FromStr` like `T` itself. Reading it checks
  the whole text and finds every parameter, so a mistake in the text
  is found before any value is there, with the line and column a
  `ParseFilterError` has always had.
- **`Template::params()`:** the names, each once, in the order they
  first stand in the text. A tool can ask for each, the admin can show
  a box per name.
- **`Template::bind(&Params) -> Result<T, BindError>`:** a `T` with
  the values in it. A template binds any number of times, and is
  `Clone`, `Send` and `Sync`. It prints with `Display` as it was read,
  `:name` and all, and reads back to the same template.
- **`Params`:** names and values, `Params::new().set(name, value)` with
  `value: impl Into<Document>`, as the builder's values are, and
  `set_value(name, &T)` for any `T: Serialize` through `to_document`
  (§13), failing as that does. Setting a name again replaces its value.
- **A bound `T` is an ordinary one.** Nothing in it remembers the
  parameters: it prints with its values, `explain` shows those, and the
  planner sees what it would see for the same text written out, so a
  bound `limit` or `starts with` gets the same plan.
- **`str::parse::<Filter>()` of a text with a parameter** stays an
  error, but says what to do: "`:status` is a parameter: read the text
  as a `Template<Filter>` and bind it". A text without parameters reads
  as a template too, and binds with an empty `Params`.

## 163.4 Binding fails
`BindError`, `#[non_exhaustive]`, says which parameter (`name()`) and
where it first stands (`offset()`), in bytes from the text's start, as
`ParseFilterError::offset` does; a value without a parameter stands
nowhere, and its offset is `None`. The values are checked before the
text is read again, in the text's order, a value without a parameter
last:
- **Missing:** a parameter of the text without a value.
- **Not used:** a value for a name the text doesn't have. Almost always
  a typo, `:staus` given as `status`, or a value meant for another text.
  A program that keeps one `Params` for several templates leaves it out
  for the ones that don't use it.
- **Not an array** where `in :list` stands, **not a count** where
  `limit`, `offset` or `size` stands.

## 163.5 How it reads
The parser (`query_text.rs`, §111) reads values in one place, `value`, and counts in
another, `count`; with a template, each meets `:name` and records it,
and with values in hand, each takes the value instead. `bind` reads the
text again that way: no new type for a filter with holes in it, and the
`Filter`, `Condition` and `Aggregate` types stay as they are. The values
are checked first (§163.4), so the second reading can't fail: the text
read before, each value fitting where it stands. Reading a
filter's text takes microseconds, against a read of a page, so reading
it again on each bind costs nothing a program would see; what the
template keeps is the checking, done once, and the list of names.

## 163.6 The command line
- **`--param name=value`,** any number of times, for `find`, `count`,
  `fields`, `aggregate`, `update` and `delete`. `aggregate`'s two texts,
  the filter and the aggregate, take the same parameters: each value is
  used by one of them or both, and one used by neither is an error.
- **The value** is read as a value of the text (§111.4): JSON, with its
  tags, or `id("…")` or `date("…")`. What doesn't read so is a string as
  written: `status=open` is the string `open`, `n=20` the number 20,
  `when='date("2026-11-01T00:00:00Z")'` a date-time, and `code=007` the
  string `007`, as JSON has no leading zeros. A string that would read
  as a value is given as JSON, `n='"20"'`; `--help` says so. A value
  that starts as JSON's string, array or object, or as `id(` or `date(`,
  and doesn't read as one is a mistake, not a string: `[1, 2` and
  `date("2026-13-01")` are typos, which a string would hide.
- **Where:** anywhere after the command, before or after the other
  arguments, `--explain` included.
- **Mistakes** are exit code 2, as a text that doesn't read is, with
  the parameter named: a `--param` without `=` or without anything after
  it, a name given twice, a value that starts as JSON or `id(`/`date(`
  and doesn't read, a `--param` on a command without a filter (`get`,
  `insert`, `info`, …), and every `BindError`, under the text's line
  with a `^` where the parameter stands. A missing value says how to
  give it: `--param a=…`. A value that no text uses names itself:
  `--param b: no text of this command has the parameter :b`.
- **The agent skill** (`skills/trunkdb/`) tells an agent to pass a
  value that comes from its user as a `--param`, never into the text.

## 163.7 Rejected
- **Positional parameters,** `?` or `$1`: counted by hand, and wrong
  when the text changes. Names say what each is, and give the admin a
  label per box.
- **`@name`,** SQL Server's: `@` is already the element itself inside
  `has` (§111.3).
- **Values that change type to fit,** `"5"` comparing as `5`: the text
  never does that, and a parameter means what its value written out
  means (§163.2).
- **Parameters for fields, sort keys or hints:** names of the program's
  own, not values from outside, and a field from outside is what the
  builder is for. Every place a parameter can't stand stays an error, so
  any of them can come later without a text changing meaning.
- **A type declared with the name,** `:n int`: the value carries its
  type.
- **A parsed filter with holes,** a `Document::Param` or a slot in each
  condition: every type and match over conditions would learn about it,
  where reading the text again (§163.5) changes the parser alone.
- **An unused value ignored:** it hides the typo that the error
  catches; a program sharing one `Params` sees which template it was.

## 163.8 Left out
- Parameters in the text of an update: there is none yet (§111.11);
  updates on the command line are JSON, which a shell variable can
  already fill.
- A parameter where a duration stands, `window at :span`.
- `trunkdb` reading parameters from a file or the environment: `--param
  name="$VAR"` does that.

## 163.9 Tests
In `tests/parameters.rs`:
- **Each place:** fifteen texts, each comparison, `in` with a
  parameter among its values and for the whole list (an empty array
  too), `not in`, `size`, `limit` and `offset`, inside `has`, bound and
  compared with the text written out: the same printed filter, and the
  same documents found.
- **A condition and an aggregate:** `having`, a column's `filter
  (where …)` and `limit`, the same rows as the text written out.
- **Values stay values:** strings holding `" or true or …`, `) or (`,
  `:title` and nothing, each finding what `Filter::eq` with it finds,
  one document at most.
- **Every kind of value:** int, the string `"5"`, float, NaN, bool,
  null, id, date-time, binary, array, object, each finding what
  `Filter::eq` finds; `5` finds the int and the float, `"5"` the
  string alone.
- **`BindError`:** missing, unused, not an array, not a count (`-1`,
  `3.0`, `"3"`, `true`), with the name and the offset of the first use,
  where a name stands twice.
- **A template:** its names once each in their order, printed as read,
  read back equal, bound on another thread 50 times, a text without
  parameters, a name set twice; `set_value` with a struct; a bound
  filter's plan the plan of the text written out.

In `query_text.rs`:
- **Random filters:** the 3,000 random filters of §111.9, printed; in
  test builds the parser records where each value, list and count
  stands, and three in four of them become parameters: read as a
  template, printed as its text, and bound to what they were, the same
  filter, printing the same text. Each place over 200 times.
- **Refused places:** a field, a path's second name, a sort key, a
  selected path, a hint, `id(…)`, `date(…)`, a field inside `has`, a
  duration, a column's name and path, each at its `:`; `:` without a
  name; and a parameter in a text read as a `Filter`, `Condition` or
  `Aggregate`, naming the template to read it as.

In `tests/cli.rs`: a word, a number, a JSON string that looks like a
number, a date-time and an array as values; a string that would change
the filter as text; `limit :n`; `--param` before the collection and
with `--explain`; `fields`; `aggregate`'s shared values; `update` and
`delete --dry-run`; each mistake's message and exit code 2.

Fuzzing: the `filter_text` target (§111.9) reads any text as a template
too. Reading never panics; a template prints as its text; it binds,
each parameter to `1` or, where it is a list, `[1]`, to a filter that
prints and reads back the same; a text that reads as a filter is a
template without parameters, binding to that filter. Two minutes,
6.4 million runs, nothing found.

## 163.10 Mutations
Each made by hand and the tests run:

- **A value taken from another name,** and **`in :list` bound as one
  value, the array, instead of its elements:** caught by the random
  filters.
- **A value without a parameter let through,** **a count that doesn't
  fit let through,** **a missing value bound as null,** and **the
  offset one byte past the `:`:** caught by the `BindError` tests.
- **A parameter read in a text read as a `Filter`,** and **a parameter
  let through as a field:** caught by the refused places.
- **The command's check for an unused `--param` gone,** and **every
  `--param` value read as a string:** caught by the command-line test.
