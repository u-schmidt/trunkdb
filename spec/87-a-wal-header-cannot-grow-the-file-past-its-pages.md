# 87. A WAL header cannot grow the file past its pages (`storage/file.rs`)

Found in the same pass as §86, and tested: a WAL whose header image
counts 2^36 pages, with one page image at the end of that range, was
restored without a word. The restore wrote that page at offset
2^49, and the database file became a sparse file of 512 TiB that
`info` read as valid. `check`, `compact` and a backup of it would walk
or copy every page of that length. Only a WAL a crash didn't write gets there: `restore_pages` checked the
page ids against the header, but the header was the WAL's own claim, and
the WAL's CRC is no protection, since whoever writes the WAL can
compute it.

## 87.1 The bound
Allocating a page past the end of the file writes it at once (§19, in
`allocate_page`), so a batch that grows the file by n pages logs those n
pages. The pages a WAL holds are therefore an upper bound for the growth
of every header in it: no header in the WAL counts more pages than the
file has, in whole pages of its length, plus the number of page images
in the WAL.

`restore_pages` refuses a header over the bound, before it writes
anything, with "the WAL's header counts N pages, more than the file and
the WAL can account for (M)". The WAL is left as it is, as for a page
past the end (§55).

The file's length, not its header's count, is the base: the header may be
the damaged one (§85), and a growth cut short leaves the file shorter
than its header, never longer than the last state written back.

## 87.2 Rejected
- A fixed ceiling on the page count (a terabyte, say): it either rules
  out real databases or allows a sparse file of that size.
- Writing the images without the check and `check`ing the result:
  the sparse file is made by then.

## 87.3 Tests
`storage/file.rs`: a header counting 2^36 pages with a page at its end
is refused and nothing is written; a header one page over the bound is
refused with the bound in the message; the existing test of restores
(`restored_pages_must_lie_inside_the_file`) now logs the pages its batch
allocates, as a real one does. The proof of concept above, against the
fixed build: the open fails with the message, and the file keeps its
32,768 bytes.

## 87.4 Limits
- A WAL that carries many pages of its own can still grow the file by
  that many; the bound is the WAL's size, which the limit of §76 holds
  at about 16 MB by default.
- Whoever can write the WAL can write the database file. This closes one
  way to cause damage out of proportion to what was written, not a
  trust boundary.
