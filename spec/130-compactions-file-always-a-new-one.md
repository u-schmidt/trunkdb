# 130. Compaction's file, always a new one (`storage/scratch.rs`)

A fix to §108. A compaction builds its new image in a file beside the
database, `<database>.compact`, and removes the name as soon as the
file is open. On Unix it opened that name with `create` and
`truncate`. Whatever was already under the name was emptied, and the
open followed links:
- **a regular file there:** emptied;
- **a symlink there:** the file it points to emptied;
- **a hard link there:** the file it shares emptied.

Removing the name afterwards removed the link, but its target was
already empty.

A robustness review found it (2026-10-26). It made `<database>.compact`
a symlink to an unrelated text file and called `compact` on a database
that was already as small as it could be. The call returned
`Ok(Compacted { pages_before: 4, pages_after: 4 })`, and the text file
was empty. A compaction always builds its image before it knows whether
anything is gained, so the file was emptied on every call.

It takes something under that name first. trunkdb never leaves its own
file there on Unix. So it takes a user who named a file that way, or
someone with write access to the directory who wants a file emptied. A
link to the database or its WAL would have damaged the database.

§108 chose to empty and reuse a file left under the name. Windows
already made the file new (`create_new`) under the first free name, so
as not to empty an image still being read (108.1).

## 130.1 The fix
- **Unix makes the file as Windows does:** `create_new`, under the
  first of `<database>.compact`, `.compact.1`, `.compact.2` and so on
  that nobody holds. `create_new` fails on any name that is already
  there, a link to nothing included, and follows no link.
- **Only the name made is removed.** What was under the other names
  stays as it was.
- **One loop for both:** `first_free` tries the names and takes the
  options to open with. Windows adds `FILE_FLAG_DELETE_ON_CLOSE`, and
  Unix removes the name it gets back.

A file left under the name on Unix now stays there, as it already did
on Windows. trunkdb can only leave one there by a crash between
opening the file and removing its name, and that file is empty.

## 130.2 Rejected
- **Opening with `O_NOFOLLOW`:** it refuses only a link, and only in
  the last part of the path. A regular file or a hard link there would
  still be emptied.
- **Refusing the compaction when the name is taken:** a stray file
  would then block every compaction until someone removed it by hand.
  Passing it by does no harm.
- **A random name, as `tempfile` makes:** with `create_new` a name
  known in advance is safe, and one next to the database's own name
  says what the file is.
- **Building the image only where it gains something:** §93 can work
  out beforehand what a compaction would leave. That would spare only
  the case where nothing is gained. A compaction that gains something
  would still empty the file.

## 130.3 Tests
- `a_compaction_leaves_what_is_under_its_images_name` (`compact.rs`):
  a symlink to someone's file under `<database>.compact` (on Windows, a
  file there). One compaction shrinks the file and a second gains
  nothing. The file, and the link to it, are as they were, and the
  database reads and checks as before. This is the review's case.
- `a_link_under_the_name_is_passed_by` (`scratch.rs`, Unix): a symlink
  to a file, a hard link to it and a symlink to nothing, under the name
  and the next two. An image is still made and read. None of them
  changes, nothing appears where the dangling link points, and nothing
  is left behind.
- `two_images_live_at_once_keep_their_pages`, changed: the other
  program's file under the name is left as it was on every platform
  now, where on Unix it was emptied and removed.

## 130.4 Mutations
| Mutation | Result |
|---|---|
| The code before (`create`, `truncate`) | caught by all three |
| `create`/`truncate` in `first_free`'s options | caught by all three |
| The given name removed, not the one made | caught by all three |
| No numbered names | caught by all three |

## 130.5 Limits
- Between the file being made and its name being removed, someone with
  write access to the directory can put something else under that
  name, and the removal removes that instead. It takes nothing away
  from the file itself.
- The Windows branch wasn't changed beyond sharing the loop. It is
  checked by CI, not run here.
