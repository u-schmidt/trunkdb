# 110. A group's first document found late (`aggregate.rs`)

A fix to §107, found by the `queries` target (§105) in CI, on its
random programs of one run (seed 39256508211, program 121).

A row shows its group's values from the group's first document by id
(§107.2), and `5` and `5.0` are one group. Where an aggregate is
answered from an index whose group fields aren't its last, a group's
entries don't come in id order: the fields after the group's order
them first. So the walk can meet the group's first document by id
late.

`Groups::add_entry_to_last` takes an entry into the group the last one
went to. When its document came before the group's first so far, it
made that document the first, its order and place, but kept the value
held: the earlier match's, as it was stored. Where that match had come
from its document, the row showed its number, `3` say, where the
first document by id holds `3.0`.

Now a number held is marked as not as stored when the first document
changes there, and the row reads it from the new first document, as it
does for a group's number taken from a key (§107.5). Any other value
equal to it is the same value, and is kept.

## 110.1 Tests
`collection.rs`: an index on `(a, b)` and three documents in group
`a = 3`: `3.0` first by id, with the last `b`; `3` with a cut `b`,
whose entry can't answer and whose document is read; `3` with a short
`b`. The row has `3.0`, as `Aggregate::apply` gives it; two matches
come from their entries, and two documents are read, the cut one's
and, for its number, the first one's.

The failing program, and 1,200 programs of three other seeds, pass.

## 110.2 Mutations
Each caught: the fix left out (the test above); every value marked
as not as stored, a string too (`an_aggregate_reads_documents_only_where_entries_cannot_tell`,
which counts the documents read).
