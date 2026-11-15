# 201. Python wheels in CI (`bindings/python/`, `.github/workflows/ci.yml`)

Step 4, the last, of the Python bindings (§197.11): a wheel built on
each system, the tests run against it, and the docs: the module's
README, the main README, the GUIDE and the agent skill. No PyPI
upload (§197.9).

## 201.1 The CI job
`python`, on Linux, macOS and Windows, each with Python 3.11 and 3.14,
the oldest the wheel serves and the newest:

- **A wheel, built with maturin** in release, from `pip install
  "maturin>=1.9,<2"`, as `pyproject.toml` asks. One `abi3` wheel, the
  same whichever of the two builds it.
- **Into a fresh virtual environment,** with pytest and mypy, and
  tested there, not against the source: the job first says where
  `trunkdb` was imported from, and fails unless it is `site-packages`.
  The tests run from `bindings/python`, which has no `trunkdb` of its
  own: the package's source is in `bindings/python/python/`.
- **The stubs against the installed module:** `stubtest` and `mypy
  --strict` on the package, from `tests/`, so the installed package is
  the one checked.
- **The wheels kept** as the run's artifacts, `trunkdb-wheel-<system>`,
  from the 3.11 jobs; the 3.14 jobs build the same wheel again only to
  test it.
- **A shell of `bash` on all three,** so one script serves Windows too:
  only the venv's `Scripts/` against `bin/` differs.

The job with the oldest Rust (1.89) builds the module's crate too.

## 201.2 What the first runs found
**A test that a release build outran.** The test that the GIL is let go timed a `count` over 40,000 documents
and refused to judge one shorter than 20 ms. The module's tests had run
against a debug build until now; against the release build CI makes,
that `count` takes 16 ms, and the test failed, each time, on "too short
to tell". It now adds 40,000 documents at a time, up to six times, until
the `count` takes 80 ms; then it times it as before. A `count` holding
the GIL still fails it in release (201.5).

**A Windows error taken for an errno.** An I/O error became Python's
`OSError(errno, message, path)`, which picks its subclass by `errno`.
On Windows, Rust's raw code is the system's, not an errno: opening a
file in a folder that isn't there gave 3, "the path not found", which
as an errno is `ESRCH`, so Python raised `ProcessLookupError` where
Linux and macOS raise `FileNotFoundError`. On Windows the code now goes
as `winerror`, `OSError(0, message, path, code)`, and Python turns it
into its errno and subclass, as its own calls on Windows do. The test
of a missing file now also asks for `errno` `ENOENT`, on all three.
The module's crate was linted for Windows before the push
(`--target x86_64-pc-windows-msvc`).

## 201.3 Installing
Two ways, both in the module's README:

- **A wheel from a CI run's artifacts,** `pip install` of the `.whl`.
- **`pip install` from the repository,** `git+…#subdirectory=bindings/
  python`, which needs Rust 1.89: maturin builds the module with the
  library beside it, a path in the same clone. Tried locally the same
  way, `uv pip install ./bindings/python` into a fresh environment on
  Python 3.12, then the README's examples run against it.

`pyproject.toml` takes the module's README as the package's
description.

## 201.4 The docs
- **`bindings/python/README.md`:** the example of §197.3; installing;
  documents, errors, closing and threads, changes as they happen; how
  to test it. Shaped as the C# wrapper's README is, so the two read
  alike.
- **README.md:** "From Python", before "From C and other languages":
  what the module is, how to get it, a short example.
- **GUIDE.md** is the Rust guide: its opening points to the Python and
  C# READMEs, rather than having a second guide inside it.
- **The agent skill:** a section "From Python" for code on a trunkdb
  file, or where the command isn't installed and the module is, and
  `references/python.md` mapping each command to its call, with the
  same care: a count before a find, a limit, a write tried with
  `dry_run=True` and confirmed. A read-only open, so an agent looks into
  a file an application has open.

Every example in the READMEs and in `references/python.md` was run
against the installed wheel.

## 201.5 Checked
- **pytest, 86 tests,** against a debug build (`maturin develop`) and
  against a release wheel installed from the folder.
- **`stubtest` and `mypy --strict`,** from `tests/`, against the
  installed wheel: none.
- **The workspace's checks,** the oldest Rust with the module's crate.
- **Mutation:** a `count` that keeps the GIL (`py.detach` left out of
  the reads), built in release: caught by the reworked test.
- **The workflow itself** runs only on GitHub: Windows and Linux were
  first tried there, and found the error of 201.2.

## 201.6 Rejected
- **`PyO3/maturin-action`,** with manylinux containers: a wheel that
  runs on older Linux than the runner's; worth it when wheels go to
  people on other systems, through PyPI. Until then the wheel is built
  on the runner as it is.
- **A wheel for every Python version:** `abi3` makes one serve them all
  (§197.12).
- **A Python section in GUIDE.md:** the guide teaches the library
  through Rust; a second language inside it would double every example.
