# 92. Typed format errors (`lib.rs`, `storage/file.rs`, `durability/wal.rs`, `database.rs`)

A program that opens files it didn't make — an admin tool, a file
browser, a backup check — has to tell four things apart: this isn't a
trunkdb file; it is one, but this build can't read it; it is one and
it's damaged; the open failed for some other reason. Until now the
first three all came back as `Error::Io` with `ErrorKind::InvalidData`,
and only the message said which. A message is for a person: matching
its words breaks without a sound the day they change. TrunkDB Admin did
exactly that, for want of anything else.

`Error::Format(FormatError)` names the first two, and leaves
`InvalidData` at an open to mean damage.

## 92.1 `Error::Format`
```rust
#[non_exhaustive]
pub enum FormatError {
    NotTrunkdb,                                      // another program's file
    Unversioned,                                     // format 0
    Newer { found: u32, current: u32 },              // open it with a newer trunkdb
    Older { found: u32, oldest: u32, current: u32 }, // export with the old build
    WalNotTrunkdb,                                   // the WAL isn't trunkdb's
    WalVersion { found: u32 },                       // a WAL version this build doesn't read
}
```
`Error` gains one variant, `Format(FormatError)`. Its `Display` is the
message from before, word for word, so the `trunkdb` command and anyone
who prints the error see what they saw.

The cases are exactly the places that built these messages:
`Header::decode` and the short-file check in `FileStore::from_file`
(`storage/file.rs`), and the WAL header check in `durability/wal.rs`.
Nothing else is a `FormatError`.

`current` is the build's `FORMAT_VERSION`, and `oldest` the oldest
format it reads as it is, the first of `COMPATIBLE_OLDER_FORMATS`. So a
caller can say "this file has format 12, this build reads 6 to 11"
without knowing either constant.

## 92.2 How it travels
Inside trunkdb an open works in `io::Result` all the way down. A
`FormatError` travels in an `io::Error` of kind `InvalidData`
(`FormatError::into_io`), as `storage::TooOld` does (§83), and
`From<io::Error> for Error` takes it out again, beside the `TooOld`
case. Nothing between the place that finds the problem and the public
API changed, and every public call returns `crate::Result`, so a caller
always sees `Error::Format`, never the wrapped form. Code inside trunkdb
that looks at an open's error kind still sees `InvalidData`.

A file that is ours, by its magic and format version (`is_ours`), but
whose header is damaged is still put aside until recovery has had its
chance (§85). So `NotTrunkdb`, `Unversioned`, `Newer` and `Older` are
reported at once, before the WAL is opened, as before.

## 92.3 What stays as it is
- **Locked.** A file another open holds is `Error::Io` of kind
  `WouldBlock`, for both opens (§21.1, §88.1). That is already a type,
  and the `trunkdb` command matches on it. `Database::open` now says so
  in its documentation, with the other cases; no variant is added.
- **Damage.** `InvalidData` from an open now means damage only: a
  checksum that doesn't match (§40), a header counting pages it can't
  have (§55), a broken WAL record with another behind it (§91.1). A
  caller may say "damaged" for it without reading the message.
- **A page size not this build's** is damage too, as it was. trunkdb has
  only ever had one page size, so a header with another one is a header
  with a damaged field, and it waits for the WAL's image of the header
  like any other (§85).
- **Missing and empty files.** A read-only open of a missing file is
  `NotFound`; an open that writes creates it. An empty file is a new,
  empty database (§22.1). A file shorter than a page whose bytes start
  like the magic is a first write-back cut short (§22.1); one that
  doesn't is `NotTrunkdb`.

## 92.4 Compatibility
`Error` is `#[non_exhaustive]`, so a new variant compiles everywhere.
What changes is behavior: code that matched `Error::Io` with
`InvalidData` to catch a foreign or unreadable file no longer sees
these cases there. That is the point of it, and it goes into 0.20.0
beside WAL version 3 (§91), which already makes that release one with a
compatibility note. No file format changed; the command's output and
exit codes are the same.

## 92.5 Rejected
- **Leaving it inside `Error::Io`**, as a payload a caller downcasts
  (`e.get_ref()?.downcast_ref::<FormatError>()`). No change in behavior
  at all, but every caller writes the same unwrapping, and trunkdb
  already turned its other typed I/O case, `TooOld`, into a variant of
  its own.
- **A variant per case on `Error`** (`Error::NotTrunkdb`,
  `Error::FormatTooNew`, …): six variants for one question, "can this
  build read this file at all". Grouped, a caller handles the family in
  one arm and looks inside only if it cares.
- **`FormatError::PageSize`**: the message read like a format question,
  but no build ever had another page size, so in practice it is damage
  (92.3).
- **`Error::Locked`**: tidier to read, but `WouldBlock` already says it
  in a type, and a new variant would move it out from under every
  caller that matches today, the command included.
- **Typing damage too** (`Error::Damaged { page }`): most damage is
  found by reads long after the open, in many places. That's a larger
  change of its own, and `check` already reports damage as data.
- **A probe that reads only the header**, `Database::identify(path)`,
  without opening: handy for a file dialog's preview, but a second
  reader of the header that must agree with the open. Possible later,
  on the same `FormatError`.

## 92.6 Tests
`database.rs`, each case through `Database::open` and a read-only open,
which agree, and with the file's bytes unchanged:
- two pages of another program's bytes, and a file of eight bytes:
  `NotTrunkdb` (and `opening_a_foreign_file_changes_nothing` now checks
  the variant too);
- format 0, 12 and 5: `Unversioned`, `Newer { found: 12, current: 11 }`,
  `Older { found: 5, oldest: 6, current: 11 }`;
- the WAL replaced by another program's bytes, and by a WAL header of
  version 4: `WalNotTrunkdb` and `WalVersion { found: 4 }`, the WAL left
  as it was; with the WAL gone, the file opens with its data;
- every message is the one from before, pinned word for word;
- a page size of 4096, and a damaged header checksum: `Io` of kind
  `InvalidData`, not `Format`; a file held by an open: `Io` of kind
  `WouldBlock`, for both opens.

`storage/file.rs` and `durability/wal.rs`: the tests of the rejected
format versions, the short foreign file and the refused WAL versions
also check that the `io::Error` carries the `FormatError`.

`tests/cli.rs`: `info`, `check` and `compact` of another program's file
and of a format 12 file print exactly `trunkdb: <file>: <message>` as
before, exit with 1, and change nothing.

Mutations, each caught: `From<io::Error>` not taking the payload out
(seven failures); the short foreign file, the WAL's magic and the WAL's
version each back to a plain `InvalidData`; a newer format reported as
older; `oldest` off by one; a message changed by a comma; a page size
mismatch made a `FormatError`.

## 92.7 Limits
- Builds up to 0.19.0 stay as they are: a WAL of version 3 left by a
  crash is refused there by message only (§91.2).
- Damage stays one kind, `InvalidData`, with the details in the message.
- No probe without opening (92.5).
