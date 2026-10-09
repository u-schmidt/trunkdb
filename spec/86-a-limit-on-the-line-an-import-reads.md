# 86. A limit on the line an import reads (`export.rs`, `src/bin/trunkdb.rs`)

Found in a pass over what reads untrusted bytes: `import` read its input
with `BufRead::lines`, which holds a whole line before anyone looks at
it. A 400 MB file with no newline took 469 MB of memory and then failed
at its first character; an input with no end, from standard input
(`import db -`), takes memory until the process dies.

## 86.1 The limit
`ImportOptions::max_line_bytes`, 256 MiB by default, `usize::MAX` for
none (as `OpenOptions::snapshot_memory`, §83). `Database::import_with`
takes the options; `import` is `import_with` with the default. A longer
line fails the import with `Error::Import`, naming the line, once the
reader has seen one byte too many, not at the end of the line.

`ImportOptions` is made with `default()` and setters, and is
`#[non_exhaustive]`, like `OpenOptions` (§60).

## 86.2 The command
`trunkdb import <file> <in.jsonl> --max-line <bytes|none>`. `none` turns
the limit off, for an input one trusts.

## 86.3 Why 256 MiB, and why not a document size
A document can be stored up to `u32::MAX` bytes (§20), and its line is
longer than that: a third more for binary in base64, and the field names.
A limit nearer an ordinary document would turn away an export of one's
own data. 256 MiB is above any line an export of ordinary documents
writes, and low enough to stop a runaway input on most machines.

Rejected: a maximum document size (as MongoDB's 16 MB). It would bound
memory everywhere, but it is a decision about what the database stores,
breaks files with larger documents, and is better made before 1.0 for
its own reasons than as a fix to import. It stays open.

## 86.4 Tests
`export.rs`: lines with `\n`, `\r\n` and no final newline are read; a
line one byte over is refused with its number; an endless reader is
refused after the limit. `tests/cli.rs`: `--max-line 60` stops the
import at line 2, a bad value is an error, `none` and a large value
import. Measured: the 400 MB line now fails with the limit's
268435456 bytes in the message and 270 MB of memory in use, where it took
469 MB; a longer input would take the same.

## 86.5 Limits
- The limit is per line, not per import; a file of many lines is read as
  before, a batch at a time (§30.3).
- A line under the limit is still held in full, and its parsed form
  takes a few times its size.
