# 186. A leaf split around its old anchor (`index/btree.rs`)

A fix to §171. The random programs of the `queries` fuzz target
(§105.4), seed 43102641193 on CI, found an insert that failed with
"index leaf 3 can't be split so that each half fits": the batch was
refused, and every later insert of a key into that leaf would have
been too. Nothing was damaged; the leaf stayed as it was.

## 186.1 Why no point fit
§171.2 gives each half of a split an anchor of its own, its middle
key, and tries every point. It said a point always exists, since a key
is at most a quarter page. That holds for whole keys, not for keys
stored after an anchor: a half's middle key can pack it much worse
than the leaf's anchor did.

The leaf held 58 entries, the new key making 59. A dozen were short
keys; the others were long, up to 1,024 bytes, nearly all `y`, many
sharing hundreds of bytes with the leaf's anchor and so stored in a few
bytes each. Split at any point up to 25, the right half didn't fit
around its middle key; from 27 on, the left half didn't, its middle
key a short one at first, which shares nothing with the long keys
beside it. At 26, neither half fit. Each half fit with the old anchor.

## 186.2 The old anchor as a second choice
Where a half doesn't fit around its middle key, it is now built around
the leaf's old anchor before the point is passed over. An anchor needn't
be one of its leaf's keys: a remove already leaves it when its key goes
(§171.2), and entries keep only how much of it they share. A plain leaf
of a file before format 14 has no anchor; its second choice is the empty
one, which stores every key whole.

With it a point always exists, for a leaf that was packed:
- every entry but the new one fit around the old anchor, so a half
  without the new key fits around it too;
- the two points beside the new key leave it with the entries before
  it or with those after it, and one of those holds at most half the
  old bytes. With the new key, at most 1,024 bytes and a few more, that
  is under a page.

The middle key stays the first choice: it is what packs a half best in
the usual case, a sorted run. The second costs nothing unless the first
fails. No format changes: such a leaf is a packed leaf like any other.

## 186.3 Tests
`a_leaf_splits_where_no_half_fits_its_own_middle_key` in `btree.rs`
builds the leaf the fuzzed program built, entry for entry, around its
anchor; checks that no point fits with the halves' own anchors, so the
test still tests this if anchors change; inserts the new key; and finds
every entry by lookup and by scan, the root now a branch.

Mutation, caught: the second choice taken out, so a half is built
around its middle key only (the test fails as CI did). The fuzz seed
runs its 400 programs clean.
