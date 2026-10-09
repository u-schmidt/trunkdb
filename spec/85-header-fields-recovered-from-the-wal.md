# 85. Header fields recovered from the WAL (`storage/file.rs`, `database.rs`)

Found in a report on §40.4: opening a store decoded the main file's
header, and refused its fields, before the WAL was opened. §40.4 had
moved only the checksum behind recovery, so a torn header write (fields
new, checksum old) recovered. A header whose fields were invalid, a page
count of 0 for one, failed the open although the WAL held the page that
replaces it. The test of §40.4 damages the checksum sector and leaves
the fields decodable, so it never met this.

A torn write does not produce it (the fields sit in the first sector);
bit rot or another program's write can. The cost of recovering from it
is small, so it is recovered.

## 85.1 What waits, and what does not
`Header::decode` checks, in order: the magic, the format version, the
page size, the page count and the free-list head.

- A bad magic or a format this build does not read is reported at once,
  as before. That file is not ours, or is another version's, and the
  WAL is not opened beside it (that would create a `.wal` next to a
  file that is not ours).
- Invalid fields in a file that has this build's magic and a format it
  reads are held in `FileStore::header_error`, and the store carries a
  placeholder header until recovery has run.

## 85.2 Recovery decides
- `restore_pages`: if the WAL holds a header image (page 0), restoring
  it replaces the damaged page, the header is read again and
  `header_error` is dropped. If the WAL holds pages but no header image,
  the held error is returned before anything is written, not the
  misleading "past the end of the file" the placeholder would give.
- `check_header`, which runs after recovery, returns the held error when
  nothing replaced it. A damaged header with nothing to recover from is
  damage, as in §40.4.

## 85.3 Rejected
- Decoding the WAL's header image first and the file's header only if
  the WAL has none: two orders of reading for one outcome, and the
  checksum check of §40.4 would need the same split.
- Deferring every header error: see §85.1.
- Repairing the header from the file's own content (the catalog, the
  page count by file length): a guess, where the WAL has the answer.

## 85.4 Tests
`database.rs`: a database with a pending WAL whose header has a page
count of 0 opens, shows the batch and passes `check`; the same damage
with nothing in the WAL fails with "header counts 0 pages". The test
fails without the change.

## 85.5 Limits
- Only a batch that changed the header logs its image: one that grew or
  shrank the file, or changed the free list. A WAL without one cannot
  repair the header, and the open fails as before.
- The WAL's own damage is not covered: a header image in a torn record
  is not restored (§16.6).
