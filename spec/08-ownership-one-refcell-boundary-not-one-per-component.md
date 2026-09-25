# 8. Ownership: one `RefCell` boundary, not one per component

*Updated by §27: the boundary is now one `RwLock` inside an `Arc`, and
`Collection` holds a clone of the `Database` handle instead of a borrow.
The reasoning below — one narrow interior-mutability boundary, plain
`&mut dyn PageStore` parameters beneath it — is unchanged.*

Decided and implemented: `Index`/`Collection` don't hold their own
`Rc<RefCell<_>>` handles into storage. Instead, `Database` holds the
crate's one interior-mutability boundary — `store: RefCell<FileStore>` —
and everything below it (`Index` trait methods, `Collection`'s method
bodies) receives `&mut dyn PageStore` (or `&dyn PageStore`) as a plain
parameter for the duration of a single call, the same way
`TransactionManager::apply_batch` already receives its `apply` closure
rather than owning callback state.

Why not `Rc<RefCell<_>>` per component: `RefCell`'s aliasing rule (one
mutable borrow, or any number of shared borrows, never both) is checked at
*runtime* — a conflicting borrow panics instead of failing to compile.
Spreading it through every internal type gives up exactly what Rust's
compile-time borrow checker is for. The mature pattern is the opposite
instinct: localize interior mutability to one deliberately narrow
boundary, and keep everything else ordinary, compile-time-checked
ownership.

Why *some* interior mutability is still needed, rather than none: with a
plain `store: FileStore` field and no `RefCell`, `Database::collection()`
would need `&mut self` to let `Collection` reach in and call `FileStore`'s
`&mut self` methods — and holding one `Collection` handle would then block
getting a second one (e.g. `"users"` and `"posts"` open at the same time),
since Rust disallows two overlapping mutable borrows of the same value.
The single `RefCell` absorbs that: `Database::collection()` stays a
shared-`&self` method, so multiple lightweight `Collection` handles can
coexist, while the controlled, single-point-of-truth mutation still
happens underneath. `database::tests::two_collections_coexist` is the
compiled proof of this.

Concrete consequence: the `Index` trait's methods (`insert`/`remove`/
`lookup`/`scan`) were widened to take a store parameter — a deliberate
signature change made now, while `InMemoryIndex` (which ignores the
parameter) was still the only implementation, rather than something a real
persisted index would have forced retroactively.
