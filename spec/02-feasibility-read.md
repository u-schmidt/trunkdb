# 2. Feasibility read

Stacking "learn Rust" and "learn database internals" simultaneously is the
real risk — not raw difficulty. Precedent that this scope is solo-feasible:
SQLite itself started as a solo project; `sled`, `redb`, and especially
[PoloDB](https://github.com/PoloDB/PoloDB) (a MongoDB/LiteDB-shaped,
single-file, embedded document database, in Rust, built solo) are direct
existence proofs for the exact shape of this project.

Decision: treat this explicitly as a learning project first, a personal
tool second, and "maybe publish to crates.io later" as a real but
non-blocking stretch goal — confirmed: the crate name `trunkdb` is
currently unclaimed on crates.io.
