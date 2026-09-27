# 72. Skip, for paging (`query.rs`, `collection.rs`, `cursor.rs`)

A list shown a page at a time needs "the third page of 20": the 41st
to the 60th match. A `limit` gives the first 20 (§34.2); nothing gave
the ones after them. Now `Filter::skip` does:

```rust
let page = |p: usize, size: usize| Filter::new()
    .eq("status", "Queued")
    .sort_desc("created")
    .skip(p * size)
    .limit(size);

let third = jobs.find(page(2, 20))?;
let total = jobs.count(Filter::new().eq("status", "Queued"))?;  // for "page 3 of 12"
```

## 72.1 What it does
- **Where:** after the sort, before the limit, as SQL's `OFFSET`,
  LINQ's `Skip` and MongoDB's `skip`: conditions, sort, skip, limit.
- **`skip(n)`** replaces an earlier skip, as `limit` does; `skip(0)`
  is none, the default. A skip past the last match gives nothing, not
  an error. It takes a `usize`, not an `Option` as `limit` does: no
  skip and a skip of 0 are the same.
- **Without a sort** the order of matches is unspecified, so
  which ones a skip passes over is too, and the pages of an unsorted
  filter may overlap or miss documents. A paged list wants a sort.

## 72.2 Everywhere a filter goes
- **`find`, `find_one`, `cursor`:** the matches after the skipped;
  `find_one` with `skip(7)` is the eighth. An unsorted `cursor` still
  streams, passing over the first matches as they come.
- **`count`:** what `find` would return, as it already counted at most
  the `limit`: the matches less the skip, at most the limit. The total
  behind a page is `count` of the filter without skip and limit.
- **`delete_many`, `update_many`, `update_fields`:** the documents
  `find` returns, skip included, as they take the sort and limit
  (§37, §38, §68).
- **`upsert`:** sort, skip and limit don't matter; it looks for the
  one match of the conditions (§29.3).
- **`explain`:** a skip changes no plan.

## 72.3 Through an index
With a sort, a limit and an index on the sort field, `find` walks the
index in order and stops after the limit (§34.2). With a skip it walks
as far as skip plus limit, then drops the skipped: page 3 of 20 reads
60 documents, not the whole collection. Skip plus limit saturates, so
`skip(usize::MAX)` is empty, not an overflow.

The skipped documents are still read, and checked against the filter:
an index entry doesn't say whether its document matches the rest of
it. So a deep page costs as much as every page before it, as a deep
`OFFSET` does in SQL. For many pages, start each page after the last
value of the one before:

```rust
let next = Filter::new().gt("created", last_created).sort_asc("created").limit(20);
```

With an index on `created`, the walk starts at `last_created` (§34.2),
and each page reads 20, however deep. This needs a sort value that is
unique, or ties at a page's edge are lost; `skip` needs none of that.

Rejected:
- **`skip` in `count` ignored, as MongoDB's `countDocuments` does by
  default:** `count` already honored `limit` as "how many would
  `find` return"; a count that honors one and not the other would
  mean neither.
- **Skipping index entries without reading their documents:** only
  right where the index answers the whole filter, every condition on
  the sort field and exact. Possible later, as a narrower fast path;
  paging by the last value serves deep pages better anyway.
- **`page(n, size)` as its own method:** two lines of `skip` and
  `limit`, and it would hide what a deep page costs.
