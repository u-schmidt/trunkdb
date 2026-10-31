# 143. `except`: every field but these (`select.rs`, `query.rs`, `query_text.rs`, `explain.rs`)

`select` (§106) names the fields a read hands out. A list that shows
everything of a note but its body had to name every other field, and
name it again when one was added. §106.8 left the other way for later,
"useful against one large field … its own method": this step adds it,
`Filter::except`, BigQuery's `SELECT * EXCEPT (…)` and MongoDB's
exclusion projection. No file format changed.

```rust
let list = notes.find(Filter::new().eq("team", "core").except(["body", "meta.raw"]))?;
// [{"_id": ..., "title": "Plan", "team": "core", "meta": {"by": "Ada"}}, ...]
```
```text
select * except body, meta.raw where team = "core"
```

## 143.1 What it hands out
What the read would hand out, less the paths:
- **Named as `select` names them** (§106.2): a dotted path reaches into
  objects and stops where its value does, so through an array, a
  string or a missing field it takes out nothing; a step ending in
  `[*]` names its array whole; the empty path names the whole document,
  so `except([""])` hands out `_id` alone.
- **`_id` stays,** excepted or not: every result keeps its identity
  (§106.8).
- **The rest keeps its order:** without a `select`, the stored order,
  `_id` first, as a whole read hands it out.
- **An object emptied by it stays,** as `{}`: the stored document had
  one there. MongoDB's exclusion keeps it too. A selection's rule
  differs (an object with nothing selected under it is left out),
  because there nothing named it.
- **A second call adds** to the first, as for `select`.

**With a `select`, it takes out of what the selection hands out:**
`select(["address"]).except(["address.zip"])` gives `address` without
its zip. MongoDB refuses to mix the two, but taking out of what's
selected is well defined, and refusing would need an error a builder
can't give until the read.

## 143.2 Where it acts
Where `select` acts (§106.3): `find`, `find_one`, `cursor` on
`Collection` and `View`, typed and untyped, `explain_run`, and live
queries (§138.4). `count` and the writes ignore it; `update_many` is
given the whole document.

- **Decoding:** without a `select`, the stored object is walked once,
  each excepted field skipped with every check a decode makes (§99.2),
  so damage reads as damage; a field excepted in part is opened where
  it is an object and decoded whole where it isn't. With a `select`,
  the selection is decoded, then the excepted paths taken out of it in
  memory: both are small by then.
- **From index entries** (§106.5): only with a `select`, as before. The
  entry answers the selection and the excepted paths are taken out of
  it; without a `select`, no entry holds every field.
- **`Explained::except`**, new: the paths, as given; empty without one.

## 143.3 As text
```
select (<paths> | *) [except <paths>] [where] …
```
`select *` is no selection, every field; `except` follows it or the
paths. `except` means something only after them, where a field can't
be, so it isn't reserved (§111.3): `except = 1` compares a field named
`except`, and `select * except except` takes one out. The eleven
reserved words stay the only ones. A filter with an `except` prints as
`select * except …`, or after its paths.

## 143.4 Rejected
- **`omit` for the method:** the text says `except`, and the builder
  says what the text says (§111).
- **Leaving `_id` out:** §106.8.
- **Refusing a `select` beside it,** as MongoDB does: 143.1.
- **Trimming arrays,** `items[*].sku` taking `sku` out of each element:
  as for `select`, a design of its own, still open (ROADMAP.md).

## 143.5 Tests
- `select.rs`:
  - `an_except_takes_out_what_it_names`: the rest in its stored order;
    nested, an object emptied kept; through an array, a scalar, a
    missing field no change; `[*]` the array whole; `_id` kept; the
    empty path, `_id` alone; after a selection, in its order, a
    selected path inside an excepted one gone, one emptied kept;
  - `decoding_a_selection_is_projecting_the_whole`, with each selection
    of three also as an except, and as a selection of two less the
    third, each over the document as cells hold it now and as files
    before §59 do, its `_id` inside: decoded and projected give the
    same text;
  - `a_selection_is_damaged_when_the_whole_is`, with six excepts:
    damage fails each exactly where, and as, the whole decode fails.
- `query_text.rs`: `select *`, `except` after it and after paths, in
  any case, a field named `except` bare where a field can be, printed
  back as read; `select * except` alone, a condition without `where`
  after `*`, refused; the random round trip draws excepts too.
- `collection.rs`:
  - `an_except_hands_out_the_rest_on_every_read`: `find`, `cursor`
    sorted and not, a `View`, `find_one`, a second `except`, the empty
    path; after a `select` answered from a compound index's entries, no
    document read; without one, every document read; typed, a struct
    without the large field, and `Error::Document` for a field it
    needs; a live query cut at the start and after; `count`,
    `update_many` and `delete_many` on whole documents;
  - the randomized corpora (`selection_agrees`): a third of their
    selections except one or two paths, half of those without the
    selection, drawn apart so the corpora draw as before; every route
    must hand out the projection, `explain_run` must say the except,
    and the documents read stay the unselected filter's less those
    answered from entries. §100's corpus answers 124 from entries, more
    than the 100 it must.
- **The `queries` fuzz target** (§105): a quarter of its reads except
  one or two paths, with a selection or without, the model projecting
  through `Filter::apply`, the typed read through the text. 1,000
  programs ran 1,941 such reads, 2,000 of seed 7 3,888, all as the
  model says. It draws more of each program, so a seed no longer gives
  the program it gave.

## 143.6 Mutations
| Mutation | Caught by |
|---|---|
| The except ignored by `project` | `an_except_takes_out…`, decoding, every read, §100's corpus |
| The except ignored after a `select` | `an_except_takes_out…`, decoding, §100's corpus |
| The except ignored by `decode` after a `select` | decoding, §100's corpus |
| The except ignored by an entry's answer | every read |
| An excepted field decoded anyway | decoding, every read, §100's corpus |
| A field excepted in part dropped whole | damage, decoding, every read, §100's corpus |
| An emptied object left out | `an_except_takes_out…`, decoding, §100's corpus |
| Taking out reordering the rest | `an_except_takes_out…`, decoding, §100's corpus |
| `_id` taken out by the empty path | `an_except_takes_out…`, every read, §100's corpus |
| A stored `_id` kept by the decode | decoding, made stronger: an old file's cell |
| An except alone taken as the whole document | `an_except_takes_out…`, every read, §100's corpus |
| The text not printing it | `except_reads_after_select`, the round trip |
| The text not reading it | `except_reads_after_select`, the round trip |
| `explain_run` without it | every read, §100's corpus |

A stored `_id` kept survived at first: the decoding test encoded its
document as cells are written now, without the `_id` in it, so neither
`except`'s nor `select`'s pass over an older cell's `_id` was ever
reached. It now decodes each document in both forms.
