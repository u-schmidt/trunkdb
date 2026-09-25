# 35. A filter builder (`query.rs`, `document.rs`)

Real and tested: a `Filter` can be built one call at a time.

```rust
Filter::new()
    .eq("status", "Complete")
    .gte("seen", 10)
    .sort_desc("started_at")
    .limit(1)
```

What it replaces, still valid (the fields stay public):

```rust
Filter {
    conditions: vec![
        Condition { field: "status".into(), op: Op::Eq, value: Document::String("Complete".into()) },
        Condition { field: "seen".into(), op: Op::Gte, value: Document::Int(10) },
    ],
    sort: Some(Sort { field: "started_at".into(), order: SortOrder::Desc }),
    limit: Some(1),
}
```

## 35.1 Methods, starting from `Filter::new()`
`eq`, `ne`, `lt`, `lte`, `gt`, `gte`, `contains`, `is_null`,
`is_not_null`, and `condition(field, op, value)` for any `Op`; each adds
one condition, ANDed with the rest. `sort_asc`/`sort_desc`/`sort_by` and
`limit` replace what was there — there's one sort and one limit.
Fields are any `impl Into<String>`, dotted paths included (§31). Each
method takes `self` and returns it, so building conditionally is
`filter = filter.eq(...)` inside an `if`.

Rejected: starting points like `Filter::eq("status", ...)` next to the
chaining `.eq(...)`. A type can't have an associated function and a
method of the same name, so it would take a second set of names
(`Filter::where_eq`) or free functions (LiteDB's `Query.EQ`, which
builds a query object, with `Query.And` to combine). One starting point
and one set of names is less to learn; `Filter::new()` costs one call.

Rejected: a borrowing builder (`&mut self -> &mut Self`). It chains the
same way, but `let f = Filter::new().eq(...)` would then borrow a
temporary that is dropped at the end of the statement; the consuming
form just works, and a `Filter` is cheap to move.

## 35.2 Plain values as `Document`s
`From` implementations turn plain values into `Document`s, so a value
is `36` or `"Berlin"` rather than `Document::Int(36)`: `bool`; `i8`–`i64`
and `u8`–`u32`; `f32`, `f64`; `&str`, `String`; `DocId`; and `Option`
of any of these, with `None` as `Null` — so a typed `Option` field's
value goes into a filter as it is, and `None` finds null and missing
(§32). A `Document` can always be passed directly.

Left out on purpose: `u64` and `usize` (they can exceed `i64`; a
`TryFrom` would turn every filter into a `Result`), and `Vec<T>` (is
`Vec<u8>` an `Array` or `Binary`?). Values of an app's own types — a
timestamp struct, an enum — go through `serde_bridge::to_document`.

## 35.3 Tests
- The builder produces exactly what the struct literal spells out —
  every method, a dotted path, a replaced sort and limit.
- Every `From` conversion, including an unsuffixed literal (`i32`) and
  nested `Option`s.
- End to end through a typed collection: conditions, sort and limit,
  an `Option` value, and a built filter reading an index in order
  (§34).
- The time-series integration test (§15) now builds its filters this
  way — including one assembled from optional parameters — and needs
  only `Filter` from `query` for it.
- The doc example on `Filter`'s builder `impl` runs as a doc test.

## 35.4 Limits
- Still AND only — OR and nesting came next, in §36 (`any_of`, `and`,
  and `|`, `&`, `!` on `Condition`).
- No compile-time check of field names — they're strings, as in
  MongoDB's drivers (a closure like LiteDB's `x => x.Name` can't be
  inspected in Rust).
