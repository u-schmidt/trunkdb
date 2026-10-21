# 116. A map of its own (`map.rs`)

An object document held its fields in an `IndexMap<String, Document>`,
the `indexmap` crate's map, which keeps them in the order they came in:
a document comes back as it was written, `_id` first. The map was
right. Its type in trunkdb's API wasn't: `Document::Object(IndexMap<…>)`
made `indexmap` 2 part of what a program using trunkdb compiles
against. Two major versions of a crate are two types to the compiler,
so trunkdb moving to an `indexmap` 3 would have broken every program
that builds or reads an object, with "expected `IndexMap<String,
Document>`, found `IndexMap<String, Document>`". §60.4 saw it and left
it, rightly in 0.x, where a breaking change costs a line in the
changelog. After 1.0 it would cost a 2.0 (ROADMAP.md, "Before 1.0"), so
it goes now.

`Document::Object` now holds a `trunkdb::Map`: trunkdb's own type, the
`IndexMap` inside it and out of sight. What's inside can change without
a program noticing.

```rust
use trunkdb::{Document, Map};

let mut task = Map::new();
task.insert("title".into(), "write the spec".into());
task.insert("hours".into(), 3.into());
let doc = Document::Object(task);

let same = Map::from([("hours", 3.into()), ("title", "write the spec".into())]);
```

## 116.1 The type
A newtype, as `serde_json::Map` and `toml::Table` are: a struct with the
map as its one field, private to trunkdb. Everything a caller does goes
through `Map`'s methods and traits:
- **Methods:** `new`, `with_capacity`, `len`, `is_empty`, `get`,
  `get_mut`, `contains_key`, `insert`, `remove`, `retain`, `clear`,
  `iter`, `iter_mut`, `keys`, `values`, `values_mut`.
- **Traits:**
  - `Index` and `IndexMut` by name, panicking on a missing field, as a
    `HashMap`'s do;
  - `FromIterator` and `Extend` of `(String, Document)`;
  - `From` an array of pairs, with `&str` names;
  - `IntoIterator` by value, by reference and by mutable reference;
  - `From<Map>` for `Document`;
  - `Debug`, `Clone`, `PartialEq`, `Default`.
- **Its iterators are its own too**, `trunkdb::map::{Iter, IterMut,
  IntoIter, Keys, Values, ValuesMut}`, each around `indexmap`'s, so no
  type of that crate is named in a signature. The `map` module is
  public so they can be named, as `serde_json::map` is; `Map` itself is
  at the crate's root, beside `Document`.

**No `Deref` to the inner map.** It would hand every `IndexMap` method
to every caller for one line of code, and with them the dependency the
type is there to hide.

## 116.2 The same shape as before
Moving a program over is renaming the type, as far as it can be:
- `insert`, `collect` and `extend` take a `String` and a `Document`, as
  `IndexMap<String, Document>`'s did, not anything that converts into
  them. The generic form, tried first, broke code that worked:
  `insert("pad".into(), …)` had no type left for its `.into()` to make.
  Only `Map::from([…])` takes `&str` names, for maps written out.
- **Order** as `IndexMap`'s: a new field goes last, one set again keeps
  its place. `remove` is `IndexMap`'s `shift_remove`, which keeps the
  order of the others, where its `swap_remove` would move the last field
  into the gap; order is what the map is for, so the choice isn't the
  caller's to make.
- **Equality** as `IndexMap`'s, and so `Document`'s as before: two maps
  are equal with the same names and equal values, in any order.
- **`Debug`** as the inner map prints, so a document still prints as
  `Object({"a": Int(1)})`.

Inside trunkdb, the field is visible, and code that wants an
`IndexMap` method `Map` doesn't offer (`entry`, `get_index_of`,
`swap_remove` where the order doesn't matter) reaches it there.

## 116.3 Rejected
- **`indexmap` named a public dependency**, as `serde` is: honest, and
  no work, but trunkdb's major version would follow `indexmap`'s for
  good, for a type a caller uses to hold the fields of a document.
- **Re-exporting `indexmap`** (`trunkdb::indexmap`): the caller then
  needn't add the crate, but the type is still that version's.
- **`Deref` to the inner map:** §116.1.
- **The entry API** (`map.entry(name).or_insert(…)`): it would want
  types of its own for the entry and its two cases. Left until someone
  asks; `get_mut` and `insert` do the same in two steps.

## 116.4 What it costs a program
A breaking change, for 0.x:
- `IndexMap<String, Document>` becomes `Map`;
- `IndexMap::new()` and `with_capacity` become `Map`'s;
- `shift_remove` becomes `remove`;
- `entry`, `first`, `get_index` and the other `IndexMap`-only methods
  become `get`, `get_mut`, `insert` and `iter`.

Inside trunkdb, that was 74 uses in 11 files and the fuzz crate,
nearly all of them renames. The fuzz crate no longer depends on
`indexmap` at all.

Nothing on disk changes.

## 116.5 Tests
`map.rs`:
- Fields in the order they came in; set again, in their place;
  `remove` keeping the others' order; `retain`, `values`, `iter`,
  `len`, `clear`.
- Values changed through `get_mut`, `map[name] = …`, `iter_mut`,
  `values_mut` and `&mut map`; `contains_key`, a missing `get`.
- Built by `collect`, `extend` and `from`; taken apart by value and by
  reference, in order; iterators from both ends, counting what's left;
  `Document::from(map)`.
- Equal in any order, unequal with a field missing or different;
  `Debug` as before, of the map and of the document.
- A missing field by index, to read (of a map with another field) and
  to change: a panic naming it.

Every other test of the library reads and writes documents through
`Map` now, so all of them run through it; they pass unchanged, but for
the uses of `IndexMap`-only methods in their own code.

## 116.6 Mutations
Each caught, with every test of the library run: `remove` as
`swap_remove` (2 tests, one of them `unset`'s, §68); `insert` moving a
field set again to the end (1); `Debug` as `Map({…})` (1); equality in
order only (1); `retain` inverted (1); iterators' `next_back` as `next`
(1), their size unknown (1); `from` dropping the last pair (4); `extend`
doing nothing (1); `map[name] = …` inserting a missing field (1);
`contains_key` always false (2); `get_mut` always `None` (5); `clear`
doing nothing (1); a missing field by index giving the first one's
value (1, after the test of it was given a field: on an empty map there
was no first value to give, and it panicked anyway).
