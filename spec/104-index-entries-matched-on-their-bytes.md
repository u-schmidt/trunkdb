# 104. Index entries matched on their bytes (`query.rs`, `index/key.rs`)

Where an index holds every field a filter names, its entries decide the
filter without their documents (§100). Until §104, deciding an entry
meant taking its key apart into values:
1. each named part was decoded into a scratch document
   (`Covering::fill`, `key::decode_part_into`): a number became a
   `Float`, a string was copied;
2. each value was written at its path in that document (`Path::slot`);
3. the filter's matcher checked the document, finding each path again
   and comparing with `value_matches`.

A run of equal values is decided once (§101.2), and a leaf whose ends
agree once for the whole leaf (§101.3). That covers `tenant == x`,
whose entries all share one value. It didn't cover a range of different
values, where every entry was a new decision: "count created in a
range", 1,000 entries of 1,000 different values.

Measured with a probe built like the benchmark (§48): 100,000 tasks,
the benchmark's three indexes, `count` of `created >= 5000, created <
6000` in a loop for 12 s under `sample`. It took 66.6 µs, about 66 ns an
entry. Inclusive shares of the time:

| | share |
|---|---:|
| deciding (`Decider::decide`) | 73% |
| — the matcher (`Matcher::matches`, `value_matches`, `compare`, path lookups) | 45% |
| — filling the scratch document (`fill`, `decode_part_into`, `slot`) | 16% |
| — finding the deciding bytes, the run's last decision | ~12% |
| stepping through the leaf (`Walk::step`, `get_cell`) | 14% |
| asking for a whole leaf (`Walk::whole_leaf`) | 7% |

SQLite counts the same range in 0.02 ms, redb in 0.03–0.04.

§104 decides an entry on its encoded bytes where they suffice, without
building anything. A leaf lying inside the interval the conditions
accept is then counted, or passed over, whole. It was the ROADMAP's
"matching an index entry on its encoded bytes" (§102.7, §102.10).

## 104.1 What the bytes tell
The key encoding (§28.1) promises two things, which are all §104 needs:
- **Order, not strictly.** For two values of one type, `a < b` (as
  `Filter` compares) implies `enc(a) ≤ enc(b)`. Different values may
  share an encoding only where `part_is_exact` says a part isn't exact:
  numbers beyond 2^53, strings cut to the key budget.
- **Type by the first byte.** Each type has its own tag. `Int` and
  `Float` share one, as they compare with each other. Values of
  different types never compare (`compare` gives `None`).

From the first, by contraposition: **`enc(a) < enc(b)` implies `a <
b`**, exact or not. Two values whose encodings differ are ordered as
their encodings are. Only equal bytes leave a question: they mean equal
values if the part is exact, and maybe not otherwise.

So a comparison of an entry's part `p` with the condition's value `w`,
encoded the way the key encodes it (the same string budget), has an
answer from the bytes alone (`key::compare_part`, `key::encode_for`):
- **Different tags:** they don't compare. `ne` holds, and nothing else
  does.
- **The same tag, `p ≠ enc(w)`:** they compare as the bytes do.
- **`p = enc(w)` and the part exact:** they're equal.
- **`p = enc(w)` and the part not exact:** no answer. The entry is
  decided as before, which reads the document (§100.2).

Some values don't encode: arrays, objects, binary, NaN. A compound
index holds such a field as `TAG_OTHER`. No such value compares with
anything (`compare` has no case for them), so `ne` holds and nothing
else does, on either side, the entry's or the condition's. Such entries
now decide where they used to read their document.

## 104.2 The entry matcher
`EntryMatcher` (`query.rs`, crate-internal) is the filter compiled
against an index's fields. The `Decider` builds it (`Covering::decider`),
so it serves every read that has one (§100.3, §101.2). Its conditions
(`OnBytes`) mirror `Matcher`:
- **`Compare`**: which part of the key (a field's position, or `None`
  for the id at its end), the `op`, the value, and its encoding (`None`
  where it doesn't encode);
- **`All`, `Any`, `Not`.**

A filter with `Exists`, `Size` or `ElemMatch` gets no conditions, and
the matcher decides nothing. `Covering` rules those out already, and
multikey indexes too (§100.1).

`matches(key)` takes the key apart once into its parts (`each_part`, at
most 8, §43) and answers with three values:
- `Some(true)` or `Some(false)` where the bytes decide;
- `None` where they don't, or where the key is damaged and holds more
  or fewer values than the index has fields.

`All` and `Any` combine those answers as SQL's three-valued logic does.
A false part decides an `All`, a true one an `Any`, whatever the
undecided parts would say. `Not` turns true and false around and leaves
`None` as it is.

Per condition:
- **`eq`, `ne`, `lt`, `lte`, `gt`, `gte`** as in 104.1, mapped to an
  answer by `holds`. That function is also what `value_matches` maps its
  ordering with, so the mapping exists once.
- **`contains`**: false where the part isn't a string, or the value
  isn't one. A string part with a string value isn't decided: case
  folding (§25) needs the text, and such entries go on as before.
- **`_id`**: the key's last 16 bytes (`key::doc_id`) against the id. Ids
  are exact (§59.5). A comparison of `_id` with anything but an id: `ne`
  holds, nothing else does.

## 104.3 Where it decides
`Decider::decide` keeps its order:
1. the run's last decision (§101.2);
2. the entry matcher;
3. only where that answers `None`, the scratch document filled and
   matched, as before.

The answer counts as a decision for the run either way. Every read with
a `Decider` uses it without change: `count` (`count_in`), the walk in
index order and its skip (§101.5), the candidates of an index range
(`read_candidates`), the cursor.

## 104.4 A leaf inside the interval, whole
`decide_all` (§101.3) decided a leaf whole only where its ends shared
their deciding bytes. §104 adds a second case, for **filters that accept
one interval of keys** (`EntryMatcher::interval`, `is_interval`).

Such a filter's conditions, through any nesting of `All`, are all
comparisons (`eq`, `lt`, `lte`, `gt`, `gte`) on fields of the index. A
comparison on a field after the first counts only where every field
before it has an `eq` among the conditions.
- `created >= a, created < b` on `created` is such a filter.
- `status == x, created > a` on `(status, created)` is one.
- `created > a` on `(status, created)` isn't: the keys are ordered by
  `status` first.
- `ne`, `contains`, an `Any`, a `Not` and `_id` all make it not one.

For such a filter, a leaf whose lowest and highest keys both match on
their bytes matches throughout. Take a key `k` between them:
- **The fixed fields.** Both ends' parts for an `eq`-fixed field equal
  `enc(w)`, and are exact. Keys are ordered and prefix-free (§28.1), so
  `k`'s part there is the same bytes, exact too.
- **A lower bound** (`gte w` or `gt w`). After the fixed parts, `k`'s
  part `p` is at least the lowest key's part `l`, which holds the bound:
  - `p > l` in bytes means a larger value than `l`'s (104.1), so it
    holds too;
  - `p = l` means the same bytes, which hold either because they are
    above `enc(w)` or because they equal it exactly, and so `p` does.
- **An upper bound**, from the highest key, the same way.
- **An `eq` on the field compared**: both ends exactly `enc(w)`, so
  every key between them is too.

So `decide_all` answers true for the leaf. The two ends' checks count as
decisions. Leaves whose ends don't match aren't answered by this rule:
an interval can lie inside a leaf.

What that gives:
- **`count` of a range** counts each leaf inside it by its live entries
  (§101.3), and decides only the entries of the leaves at its two ends,
  on their bytes.
- **A skip on such a filter in index order** (`created >= a`, sorted by
  `created`, page 50) passes whole leaves, as §101.5 did for `eq`.

## 104.5 What stays
- **Every result and count.** An entry decided on its bytes gets the
  answer its document would: 104.1 and 104.4 are why, and the tests
  check it against `Matcher` on the document (104.7).
- **`Matcher` and `value_matches`** are still the one meaning of each
  condition. The entry matcher only answers where the bytes make that
  answer certain, and passes everything else on.
- **The API, the file format, `explain` and `explain_run`.** The entries
  and documents examined stay what they were; only how an entry is
  decided changes.

## 104.6 Rejected
- **Replacing the scratch document entirely**, with every condition on
  bytes: `contains` needs the folded text, and an inexact part equal to
  the value needs the document. The fallback stays, and costs nothing
  where it isn't reached.
- **`contains` on the bytes:** case folding (`to_lowercase`, `ß`) over
  UTF-8 would be a second implementation of `fold_case`, for a condition
  no index narrows (§28.4).
- **Intervals for `ne`, `Any` and `Not`:** they accept several
  intervals, and a leaf with both ends inside could hold a gap. That
  would need the leaf checked against each interval's bounds. Ranges are
  the common case.
- **A leaf decided false whole by the rule** (both ends false): the
  accepted interval may lie between them.
- **Proving conditions from the walk's range** (the range is the
  conditions, so entries inside it match): the range includes a bound's
  encoding (§28.3), and covers the conditions only in part. The interval
  rule gives the same leaves, and decides the rest.

## 104.7 Tests
- **Bytes against documents**
  (`entries_decide_on_their_bytes_as_their_documents_do`, `query.rs`).
  The values are chosen to collide or sit side by side in their
  encodings (`colliding_values`):
  - 2^60, 2^60 + 1 and 2^60 + 300, the largest and smallest `i64`;
  - `Int(3)` beside `Float(3.0)`, `-0.0`, both infinities, NaN;
  - strings cut alike in keys of one field and of three, `"a\0"`,
    `"Straße"`;
  - ids, date-times a nanosecond apart, null, both bools, an array, an
    object and binary.

  Five indexes: the primary one, `a`, the nested `o.p`, `(a, b)` and `(a,
  o.p, c)`. 1,500 filters each, of one or two random conditions nested
  three deep with every op and `_id` among the fields, are checked
  against eight documents each. Fields are missing now and then. Where
  the entry matcher answers, it answers as `Matcher` on the document,
  and it answers for more than three in four.
- **The interval rule**
  (`keys_between_two_matching_ends_of_an_interval_match`): 400 entries
  of numbers, of strings, and of all encodable values, on `a` and on
  `(a, b)`, sorted by key, and 600 filters each. Of every interval
  filter, every key between the first and the last that match on their
  bytes matches; over 300 such intervals are found. Filters with `ne`,
  `Any`, `Not`, `_id`, or a range on `b` without an `eq` on `a` aren't
  intervals; `b` after an `eq` on `a` is.
- **Work done** (`a_range_is_decided_on_bytes_and_its_inner_leaves_whole`,
  `collection.rs`), 5,000 tasks on `created` and `(status, created)`:
  - a count of 2,000 different `created`: nothing read, nothing filled
    (a `#[cfg(test)]` counter, `FILLS`), decisions under half the
    entries;
  - with `status == "Done"`, on the compound index: the same, under 500
    decisions for 1,000;
  - with `ne` too, two intervals: decided entry by entry, nothing
    filled;
  - page 50 of 20 by `created >= 1000`: 20 read, no key copied, nothing
    filled;
  - an array in the compound key: decided, unread;
  - `contains` on a string: filled.
- **The corpora** (§100's `entries_decide_as_the_documents_would`,
  §101's `whole_leaves_decide_as_the_documents_would`, §102, §103,
  compound, sparse) pass unchanged.

Twelve hand-made mutations, all caught:
- different tags compared as bytes;
- equal bytes taken as equal on an inexact part;
- the value encoded with a one-field key's string budget in a compound
  key;
- `ne` across types not holding;
- `Any` decided true on an undecided part;
- `All` decided false on an undecided part;
- `Not` turning `None` into an answer;
- `_id` read at the front of the key instead of its end;
- the interval rule on a later field without an `eq` before it;
- the interval rule on `ne`;
- `contains` on a string decided on bytes;
- a leaf decided false whole because both ends fail. This one first
  survived. It needed a leaf whose ends fail with matches between them,
  which the walk's range allows because it holds the bounds' own
  encodings (§28.3): a run of 10s, five 11s and a run of 12s, counted by
  `v > 10, v < 12`. That case was added to the work test.

## 104.8 Measurements
Benchmark (§48), no new rows. trunkdb before (`ad3d184`) and after, back
to back, three runs each, on 100,000 documents:

| | before | after |
|---|---:|---:|
| `count` created in a range (1,000) | 0.07–0.08 ms | 0.01 ms |

The benchmark prints milliseconds. Measured in µs, in three more runs
each, the count takes 72.0–75.9 µs before and 14.6–15.1 µs after. In
those runs SQLite takes 20.7–29.7 µs and redb 33.7–69.0 µs, so trunkdb's
range count is now the fastest of the four. The aim was 10–20 µs, from
"count tenant == x", which counts its leaves whole the same way.

The probe agrees: 66.6 µs before, 13.1 µs after.

Every other row stays within its noise. A find of 1,000 documents is
bound by reading them (§99), and "status == x by created, page 50"
decided its leaves whole already, by its `eq`.

## 104.9 Limits
- **`contains` on a string part** still decodes the entry.
- **An inexact part equal to the value** still reads its document.
- **Filters that accept several intervals** (`ne`, `Any`, `Not`) decide
  entry by entry, on bytes, but not leaf by leaf.
- **A leaf outside the interval** isn't passed whole by the rule. A
  walk's range rarely holds one, as it comes from the same conditions.
