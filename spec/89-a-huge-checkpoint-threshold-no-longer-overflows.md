# 89. A huge checkpoint threshold no longer overflows (`database.rs`)

Noticed while building §88: the default WAL limit of §76.2 is twice the
bytes of `checkpoint_pages` pages, worked out as `2 * pages * 8192` in
`u64`. Past 2^50 pages that multiplication overflows. A debug build
panicked in `open_with` ("attempt to multiply with overflow"). A release
build wrapped round: at exactly 2^50 pages the limit came out as 0, so
every commit checkpointed and emptied the WAL, the opposite of what a
huge threshold asks for. `usize::MAX`, the obvious way to say "never by
page count", happened to wrap to a large number, so the bug showed only
in debug builds there.

## 89.1 The fix
The product is taken with `saturating_mul`. A threshold too large to
count in bytes gives a limit of `u64::MAX`, which no WAL reaches: no
limit, which is what the threshold meant. An explicit
`checkpoint_wal_bytes` is used as it is, as before. The option's
documentation says so.

## 89.2 Rejected
- **Refusing such a threshold**, or capping `checkpoint_pages` itself:
  `usize::MAX` is a reasonable way to say "only by WAL size, or by
  `checkpoint`", and it already worked for the page count.
- **`checked_mul` with an error from `open_with`**: an error for a
  value whose meaning is clear.

## 89.3 Tests
`database.rs`: `checkpoint_pages(usize::MAX)` and `checkpoint_pages(2^50)`
each leave 200 commits of one page in the WAL, with pages still
waiting. Before the fix the test panicked in a debug build, and failed
for 2^50 in a release build with a WAL peak of 0. Mutation:
`wrapping_mul` for `saturating_mul` is caught in a debug build too, by
the 2^50 case.

## 89.4 Limits
- With no limit and no page threshold, the WAL grows until `checkpoint`
  or the last handle's drop: the cost §76 describes, chosen by whoever
  sets the threshold that high.
