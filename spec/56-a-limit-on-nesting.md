# 56. A limit on nesting (`document.rs`, `collection.rs`)

Decoding a document recurses once per level of nesting, and so do
encoding, the JSON of an export, and comparing, dropping and cloning a
`Document`. §55 found that a damaged document claiming tens of thousands
of levels would overflow the stack while being read, and that crash can't
be caught: the host application ends. The fix is a limit on reading. A
limit on reading alone, though, would let trunkdb write a document it
then can't read back, so the same limit applies to writing.

**Documents nest at most 64 levels** (`document::MAX_NESTING`). They're
counted as MongoDB counts: the document is the first level, and every
object or array inside adds one. A write deeper than that is refused
the way a name too long is (§22.3): an `InvalidInput` I/O error, and the
batch rolls back. Reading stops at the same depth with `InvalidData`. No
file format changed.

## 56.1 Why 64
- **Stack safety doesn't ask for less.** One level of decoding takes
  about a hundred bytes of stack; even the 2 MiB of a spawned thread
  holds thousands. A small limit would add nothing there, only a
  stricter rule about the shape of data.
- **Real data doesn't come near it.** Arrays count, so a "conceptual"
  level often costs two: `orders.lines[*].options.color` is four. And a
  typed `Collection<T>` pays serde's wrapper for each enum variant with
  data (`{"Circle": {...}}`, §13.1), one more level each. 16 would be
  reachable by an ordinary enum-heavy model, 32 by an unusual one; 64
  leaves room for both.
- **Export and import must stay a round trip** (§30), and that's what
  rules out 128, the first choice. Import parses each line with
  serde_json, which refuses more than 127 levels (measured: 127 parse,
  128 don't). An export line is deeper than its document: a value JSON
  has no type for is written inside a tag, `{"$id": ...}`, one more level
  at the bottom; an object that looks like a tag is written inside
  `{"$object": ...}`, one more level each time. A document at 128 levels
  would export to a line import refuses.
- **Raising it later is harmless, lowering isn't.** Nothing stored
  becomes unreadable when the limit grows; everything deeper than a new,
  lower limit would.

MongoDB's limit is 100; serde_json's is 128 (127 levels).

## 56.2 Tag-like objects count twice
An object whose only key besides `_id` is an export tag (`$id`,
`$binary`, `$float`, `$object`, `$value`) is written inside `$object`,
so in JSON it's two levels. When a write is checked, it counts as two.
An export line is then at most two levels deeper than its document's
count (the line itself, for a document that isn't an object, and a tag
at the bottom): 66, well inside 127. Without this, a document of 64 such
objects would export to 129 levels. Only absurd data is affected; the
rule exists so the round trip is a guarantee, not an expectation.

Reading counts every object once. A document that passed the write
check is never deeper than that, so everything written can be read.

## 56.3 How it's checked
- **On write**: `Document::nests_too_deep`, in `apply_write_op` for
  every insert and update, so batches, `upsert`, `update_many` and
  import all go through it. It stops descending at the limit, so a
  document nested far too deep for the stack to walk whole is refused,
  not walked.
- **On read**: `decode_value` carries how many levels may still open,
  and an array or object past the limit is `InvalidData`.

Rejected:
- **A new `Error` variant.** A limit breached is a caller's mistake like
  a name too long, which is already `Io(InvalidInput)`. A new variant
  would break every exhaustive `match` on `Error`, which isn't
  `#[non_exhaustive]`.
- **Limiting only reading**, with writing unlimited: trunkdb could then
  store what it can't read.
- **A higher limit, with import parsing lines without serde_json's
  limit**: that needs its own depth check before parsing, or a line of a
  million `[` overflows the stack in the parser, for headroom nobody is
  likely to use. Possible later, since raising is harmless.

## 56.4 Tests
- `document.rs`: 64 levels encode and decode; 65 are refused by the
  check, and a cell holding them is `InvalidData`. Tag-like objects count
  twice, `_id` beside one doesn't count as a field (31 tag-like levels
  around an array are 63; at the top next to an `_id`, 65). A document
  100,000 levels deep is refused without being walked.
- `collection.rs`: an insert and an update one level too deep are
  refused as `InvalidInput` naming the limit; a batch with one such
  document rolls back whole.
- `export.rs`: the deepest documents a write allows, in both shapes that
  add the most JSON levels (an id at the bottom of 64 plain levels, and
  32 tag-like objects), export and import, and the import exports the
  same text.
- Checked by breaking it on purpose, seven ways. Each of these fails a
  test:
  - arrays not counting; tag-like objects counting once; `_id` counting
    as a field;
  - decoding without the limit; a limit of 65;
  - inserts or updates unchecked.

## 56.5 Limits
- **Files written before the limit** could hold a document deeper than
  64 levels, which would now be an error to read. Nothing is known to:
  it takes deliberate nesting that deep. Export with the version that
  wrote it, flatten, import.
- **`Document` itself isn't limited**: building one deeper in memory, and
  cloning, comparing or dropping it, recurses as before. Only storing it
  is refused.
- **The serde bridge** (§13) converts a `T` before the check sees it; a
  `T` nested past the stack fails there, in serde, as it always did.
