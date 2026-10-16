# 98. A lock held by a starting process (`database.rs`, docs)

§21.1 says the file stays locked until the last handle is dropped. On
Unix that wasn't quite all. The lock can outlive the drop by a moment,
held by a process the application is starting at the same time.

It showed in trunkdb's own tests: `looking_into_a_file_opens_it_read_only`
in `tests/cli.rs` failed once on Linux CI (run 38868095754). Right after
`drop(db)`, opening the file again found it locked. `7499bd0` fixed the
test. This section says what an application can meet.

Documentation only: no code changes.

## 98.1 What happens
The lock is the OS's `flock` (§21.1, §88). It belongs to the open file,
not to one descriptor of it, and lasts while any descriptor of that
open file exists.

A process is started in two steps: a fork, then an exec of the new
program.
- **From the fork to the exec,** the child holds a copy of every
  descriptor of its parent, the database's file among them.
- **At the exec** the copies close: std opens every file with
  `O_CLOEXEC`.
- **`Command::spawn` returns** only after the exec.

So a database dropped on one thread while another thread is starting a
process stays locked until that child's exec. An open of the file in
that window fails with `WouldBlock`, as if another handle held it:
- in the same process;
- or in the child itself, if it is a program that opens the database.

The window is usually microseconds. Under load it is longer, and a
`pre_exec` hook stretches it by whatever the hook does.

**Where:**
- **Linux:** std starts processes with glibc's `posix_spawn` (a `clone`
  without `CLONE_FILES`) or with `fork`. Both copy the descriptors.
- **macOS and other Unix:** `posix_spawn` is one step without a window,
  but std falls back to `fork` in some cases, `pre_exec` among them.
- **Windows:** not affected. std opens files with handles that are not
  inherited.

Reproduced on macOS with a `pre_exec` that waits: during the wait a
reopen fails with "database file is already open", and after the
child's exec it succeeds. `7499bd0`'s
`a_database_closed_while_a_process_starts_opens_again` forces the same
window.

## 98.2 What the documentation says
- **`Database::open`, under `WouldBlock`:** on Unix, also for a moment
  after the last handle was dropped, if another thread of this process
  was starting a process then.
- **`Database`'s type documentation,** where it says the file stays
  locked until the last clone is dropped: the same, in one sentence.
- **§21.1:** a pointer here.
- **DESIGN,** with the lock: one sentence. **README:** item 68.

What to do, in that documentation:
- **Don't reopen; keep or clone the handle.** `Database::open` already
  advised it within a process, and it avoids the window entirely.
- **Retry briefly** when an application closes and reopens on purpose
  (to compact from the command line, say) and starts processes at the
  same time. A `WouldBlock` that persists means another holder. One that
  goes away within moments was a starting child.
- **Or start processes and close databases under one lock,** as
  `tests/cli.rs` does: a close that holds it frees the file when it
  returns.

No number goes with the retry. The window depends on the machine, and a
number would read as a promise.

## 98.3 Rejected
- **Retrying inside `open`:** §21.1 makes `open` fail at once on
  purpose, and from the outside a starting child can't be told from a
  real second holder. A wait would hang a caller whose file another
  program keeps open.
- **Per-process `fcntl` locks:** a child doesn't inherit them, but any
  close of any descriptor of the file in the process releases them, and
  they don't stop a second open in the same process, which §21.1 relies
  on.
- **Open-file-description locks (`F_OFD_SETLK`):** they are copied to a
  child like `flock`, so the window stays.
- **A `close` that waits for children:** trunkdb doesn't start them and
  can't know about them.
- **An `OpenOptions` setting to wait for the lock for a while:** possible
  later, if real use asks for it. A retry loop in the application does
  the same today.

## 98.4 Tests
None new: no code changes. `7499bd0`'s
`a_database_closed_while_a_process_starts_opens_again` shows the window
and the remedy with the tests' lock. It fails without either half of
that lock. rustdoc with `-D warnings` and the doc tests pass.

## 98.5 Limits
- On Unix, a just-dropped database can be reported as held for a moment
  by an application that starts processes on other threads.
- Only the application can avoid it: by not reopening, by retrying, or
  by ordering its spawns and closes.
