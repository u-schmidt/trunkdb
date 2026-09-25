//! Writes starting inputs for the fuzz targets into `corpus/` (SPEC §55):
//! a whole database for `open_raw`, a valid WAL for `recover_wal`, an
//! export for `import`. Without them, random bytes rarely get past a
//! file's first check. `cargo run --example seed_corpus` from `fuzz/`.

use trunkdb::storage::USABLE_PAGE_SIZE;
use trunkdb_fuzz::ALL;

fn write(target: &str, name: &str, bytes: &[u8]) {
    let dir = std::path::Path::new("corpus").join(target);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), bytes).unwrap();
    println!("corpus/{target}/{name}: {} bytes", bytes.len());
}

fn main() {
    let base = trunkdb_fuzz::base();

    // open_raw: [calls][pages without checksums].
    let mut raw = vec![ALL];
    raw.extend_from_slice(base);
    write("open_raw", "base", &raw);

    // recover_wal, raw mode: [mode, top bit clear][a WAL with one record
    // of the catalog page and the first data page, as they are].
    let pages: Vec<(u64, Vec<u8>)> = [1u64, 2]
        .iter()
        .map(|&id| {
            let at = id as usize * USABLE_PAGE_SIZE;
            (id, base[at..at + USABLE_PAGE_SIZE].to_vec())
        })
        .collect();
    let mut wal = vec![ALL & 0x7F];
    wal.extend_from_slice(&trunkdb::fuzzing::wal(&pages));
    write("recover_wal", "record", &wal);

    // import: an export of the base database.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("seed.trunkdb");
    std::fs::write(&path, trunkdb::fuzzing::seal(base)).unwrap();
    let db = trunkdb::Database::open(&path).unwrap();
    let mut export = Vec::new();
    db.export(&mut export).unwrap();
    write("import", "export.jsonl", &export);
}
