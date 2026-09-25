# 42. Array conditions and multikey indexes (`query.rs`, `collection.rs`, `check.rs`, `compact.rs`)

Real and tested: a path step ending in `[*]` reaches every element of an
array. A condition on such a path holds if it holds for any element. An
index on it has one entry per element: a multikey index. File format 7.

```rust
posts.find(Filter::new().eq("tags[*]", "rust"))?;         // some tag is "rust"
posts.find(Filter::new().gt("comments[*].likes", 10))?;   // some comment has more than 10 likes
posts.find(Filter::new().ne("tags[*]", "draft"))?;        // no tag is "draft"
posts.ensure_index("tags[*]")?;                           // one entry per tag
users.ensure_unique_index("aliases[*]")?;                 // no alias shared by two users
```

## 42.1 `[*]` in a path: any element
A step is a field name followed by any number of `[*]`. Each `[*]` goes
on with every element of the array at that point, so a path reaches any
number of values (`query::values_at`). The rules:
- A missing field is null, as everywhere (§32). So a comment without
  `likes` gives a null.
- `[*]` on anything but an array gives nothing. A missing, null or
  scalar `tags` has no elements.
- A path without `[*]` is unchanged: arrays aren't walked into (§31.3).

A comparison on such a path holds if it holds for any value there;
`Contains` looks at each string element. The exception is `Ne`, which
stays what it is for one value: `Eq` negated. So `tags[*] != "draft"`
means no tag is "draft". A post without tags matches it, and matches no
other comparison on `tags[*]`.

Two comparisons on the same path may be met by different elements:
`tags[*] > 5 AND tags[*] < 3` holds for `[1, 10]`. Negating one gives
an "every element" condition: `!Condition::lt("scores[*]", 50)` means no
score is below 50.

Rejected:
- **MongoDB's implicit rule**, where `tags == "rust"` matches an array
  holding it. It would change what existing queries mean, since today an
  array equals no scalar. It would also blur the field and its elements
  into one. LiteDB marks the difference too (`$.tags[*] ANY`).
- **An operator instead of a path step** (`has(field, value)`,
  `any_element(field, condition)`). A path step works with every
  operator there is, with deeper paths (`comments[*].likes`), and in an
  index: an index is on a path, and `tags[*]` names exactly what it
  holds.
- **Numeric steps** (`items[0]`): already rejected in §31.3.

## 42.2 Multikey indexes
`ensure_index("tags[*]")` makes an index with one entry per element of
an indexed type, each value once per document. A document without
elements has no entries. A condition on elements never matches such a
document, except `Ne`, which never uses an index anyway.

§28.6's "one entry per document per index" became "a set of keys per
document" (`secondary_entries`). A write removes the keys that went away
and inserts the new ones, or all of them if the document moved (§20.3).
`find` returns a document once even when several of its elements are in
the range read: entries are deduplicated by document id, as OR unions
(§36.3) already did. `check` expects each document's set of keys, and
`compact` rebuilds from it.

A unique multikey index forbids a value shared by two documents, not
one repeated within a document: a user may list an alias twice. It
checks every element that goes in, including elements that share a key
without being equal: two long strings that differ only after the
~1000-byte key cut (§28.1) are one entry, and both are checked. The
first version deduplicated by key and checked only the first of them.
So if one user had `…b`, another could list `…a` and `…b` and get
`…b` too. I found that while choosing what to break for the mutation
checks. The test I first wrote for it inserted in the other order,
where the old code caught it anyway; a surviving mutation showed that,
and the test now tries both orders.

`ensure_index` takes brackets only as `[*]` right after a name:
`tags[1]`, `ta[*]gs` and `[*]` are errors.

Rejected:
- **Indexing the whole array as one value.** It would find only exactly
  equal arrays, never an element.
- **A null entry for a document without elements**, as §32 does for a
  missing field on a plain path. No condition that uses an index could
  ever read those entries.

## 42.3 What the planner does differently
- **Ranges aren't intersected on a `[*]` path.** Different elements may
  meet different comparisons, so the intersection of their ranges can
  miss documents (the `[1, 10]` example above). One of them bounds the
  read, an `Eq`'s if there is one, and the filter checks the rest.
- **A multikey index is never read for a sort.** It holds a document
  once per element, in element order: no order of documents. Sorting by
  a `[*]` path doesn't sort at all. The path names no single value, so
  every document reads null there and they come back in id order.

## 42.4 File format 7
A build before this one would take `tags[*]` for a field name: every
document would be missing it, so the index would get one null entry per
document, and later writes would corrupt it for this build. So the
format is 7. A format-6 file is a valid format-7 file that has no such
index: it opens as it is (`COMPATIBLE_OLDER_FORMATS`), and its next
page allocation stamps it 7, the way §33.4 introduced format 5. From
then on 0.6.0 refuses it.

## 42.5 Tests
- `query.rs`:
  - `values_at`: elements of mixed kinds, nested arrays (`[*][*]`),
    elements without the field or that aren't objects, no elements for
    scalars and missing fields, and one value without brackets;
  - every operator on elements, `Ne` as `Eq` negated, and no elements
    matching nothing but `Ne`;
  - a multikey index isn't read for a sort;
  - ranges on elements aren't intersected, and an `Eq` bounds when
    there is one.
- `collection.rs`:
  - A randomized test like §28.7's on `tags[*]` and `items[*].n`:
    documents with up to four random elements (repeats, nested arrays,
    scalars instead of arrays, items without `n`), an index built over
    existing documents and then maintained through inserts, updates
    that move documents, and deletes. 300 random filters per path must
    find what a scan finds, before and after a reopen. Each consistency
    check also runs `check` and compacts (§39.3, §41).
  - Typed posts with tags and comments: six queries give the same
    answers with and without indexes; a post with a tag twice counts
    once in `find`, `count` and `cursor`; `update_many` and
    `delete_many` keep the indexes right; export and import keep them.
  - Unique multikey: an alias taken by another user is refused, one
    given up can be taken, a user's own repeats are fine, and building
    over a duplicate fails. Long strings that share a key are all
    checked, in either order.
  - Paths that `ensure_index` refuses and takes.
  - Sorting by a `[*]` path returns each document once, in id order.
- `check.rs`: a duplicate among one document's elements under a shared
  key is reported, as the only problem.
- Checked by breaking it on purpose, thirteen ways. Each of these fails
  a test:
  - ranges on elements intersected, or bounded by the first comparison
    instead of an `Eq`;
  - a multikey index read for a sort;
  - `[*]` on a scalar giving the scalar;
  - `Ne` as "some element differs";
  - brackets anywhere in an index path;
  - no deduplication of documents in a range;
  - no re-pointing of unchanged keys when a document moves, or no
    re-insert;
  - a unique check of only the first value under a key, of only the
    other document's first element, or in `check`, of only the first
    value;
  - `check` counting a document's own repeated elements as duplicates.

  Two of them passed at first: the first-value-only checks in the write
  path and in `check`. Both now fail the tests described above.

## 42.6 Limits
- **No conditions tied to one element** (MongoDB's `$elemMatch`: "an
  item with sku A1 *and* qty > 2"). Two conditions on `items[*]` may
  be met by two different items. (Added in §46.)
- **No condition on an array's size**, and no way to ask for an empty
  array. (Added in §45.)
- **Sorting by elements** doesn't sort (§42.3).
- **A field whose name itself ends in `[*]`** can't be reached, just as
  one with a dot can't (§31.2).
