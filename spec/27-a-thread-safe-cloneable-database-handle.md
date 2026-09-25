# 27. A thread-safe, cloneable `Database` handle (`database.rs`, `collection.rs`, `batch.rs`)

Real and tested: `Database` is a handle — `Clone`, `Send`, `Sync` —
and so are `Collection<T>` and `Batch`, which no longer borrow the
database. A handle can go into an app's shared state or a `static`, be
moved into a spawned thread, or be stored in a struct next to the
collections it hands out.

```rust
let db = Database::open("app.trunkdb")?;
let articles = db.collection::<Article>("articles"); // no 'db lifetime
std::thread::spawn(move || articles.insert(article)); // needs 'static: fine now
```

## 27.1 The shape: `Arc<Shared>`, one `RwLock<State>` inside
```rust
pub struct Database { inner: Arc<Shared> }            // #[derive(Clone)]
struct Shared { state: RwLock<State>, id_gen, txn }   // what clones share
pub(crate) struct State { store, catalog, durability, poisoned }
```
Cloning is an atomic reference-count increment (C#'s mental model: a
class reference, where every copy points at one object — `Arc` is what
makes that explicit, and thread-safe, in Rust). The file closes, and its
lock (§21.1) is released, when the last handle is dropped — including
the ones inside `Collection`s and `Batch`es. `Collection<T>` holds a
`Database` clone and its name; `Batch` holds a clone and its ops;
`Batch`'s "same database?" check became `Arc::ptr_eq`.

§8's design carries over unchanged, only the boundary type changed: the
three `RefCell`s and the `Cell<bool>` became one `RwLock` around all
the mutable state, and everything beneath it still receives `&mut dyn
PageStore` / `&mut Catalog` as plain parameters.

One lock rather than one per field (`store`, `catalog`, `durability`):
a batch needs all of them together, and reads need `store` and
`catalog` together — separate locks would only add a lock order to get
right, with no parallelism to gain.

`Collection<T>`'s marker became `PhantomData<fn() -> T>`: a plain
`PhantomData<T>` would make the handle `Send`/`Sync` only if `T` is,
though the handle never holds a `T`. `Clone` is implemented by hand for
the same reason — deriving it would require `T: Clone`.

## 27.2 `RwLock`, not `Mutex`
Reads (`get`, `find`, `find_with_ids`) take the lock shared and run in
parallel; `write_batch` takes it exclusively for the whole commit,
stage to checkpoint (§19.3). That is exactly what keeps readers from
seeing staged pages: while a batch is in flight, no reader holds the
lock, so a reader sees a batch entirely or not at all
(`readers_never_see_half_a_batch`). `FileStore` reads through `&self`
with positioned reads (`pread`), so parallel readers don't fight over a
file cursor.

A `Mutex` would have been equally correct and serialized readers too; it
costs nothing to take the better of the two, since reads already only
need `&` access. What this still isn't: readers during a write. A batch
blocks all readers until it has `fsync`ed twice — tens of milliseconds.
Truly concurrent readers need MVCC or a snapshot of the pre-batch pages
(ROADMAP.md).

`find` drops the lock before filtering and sorting: the candidates are
owned copies by then. No user code (serde conversion, filter closures)
ever runs under the lock, so a handle used inside one can't deadlock on
it — `std`'s `RwLock` isn't reentrant.

The `GlobalLockTxnManager`'s own `Mutex` is now always uncontended — the
write lock already serializes batches. It stays: it's the
`TransactionManager` implementation's own guarantee (§17), and a
different implementation behind that trait (e.g. MVCC) will need the
write lock to go away, not the trait's lock.

## 27.3 Poisoning, by lock or by flag
Two ways a database becomes unusable until reopened (`Error::Poisoned`),
both checked by the one `read()`/`write()` accessor every public entry
point goes through:
- the `poisoned` flag, for a batch that was logged but couldn't be
  written back (§19.6) — now a plain `bool` inside `State`;
- a **poisoned lock**: a thread that panics while holding the write lock
  (a bug, or a corrupt file hitting an `expect`) poisons it, and every
  handle reports `Error::Poisoned` from then on. This closes §22.4's
  "panic mid-batch leaves the store staging" for free: nobody reads
  those staged pages (`a_panic_mid_write_poisons_every_handle`). A panic
  under a *read* lock can't leave anything half-done, and `std` doesn't
  poison on it.

Recovery is what it was: reopen. With handles spread across threads
that now means dropping all of them first — the file lock is held until
the last one goes.

## 27.4 Considered and rejected
- **Keeping `Collection<'db, T>`** and making only `Database` `Sync`:
  `thread::scope` would work, `thread::spawn` and `AppState` storage
  wouldn't — they need `'static`. The borrow was the actual obstacle.
- **`Arc` inside, but `Collection` still borrowing**: the worst of both.
- **Asking callers to wrap it** (`Mutex<Database>`, §5.3's interim
  answer): works, but serializes reads, and every caller writes
  the same boilerplate.
- **A `parking_lot` `RwLock`** (no poisoning, fairer to writers): a new
  dependency, and poisoning is a feature here (§27.3). `std`'s lock may
  let a steady stream of readers delay a writer on some platforms —
  acceptable at trunkdb's scale.

## 27.5 API break
`Collection<'_, T>` in signatures becomes `Collection<T>`; `Batch<'_>`
becomes `Batch`. Nothing else in the public API changed. Tests use the
new freedom: a collection handle outliving the `Database` variable it
came from, writers on four spawned threads, a reader running against a
writer, and a compile-time `Send + Sync + 'static` check.
