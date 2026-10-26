//! Writes starting inputs for the fuzz targets into `corpus/` (SPEC §55):
//! a whole database for `open_raw`, a valid WAL for `recover_wal`, an
//! export for `import`. Without them, random bytes rarely get past a
//! file's first check. For `queries`, programs long enough for their 64
//! steps (SPEC §105.4): libFuzzer's own inputs start short. `cargo run
//! --example seed_corpus` from `fuzz/`.

use trunkdb::fuzzing::USABLE_PAGE_SIZE;
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
    // The catalog page's slot directory past the page's end, every slot
    // empty, cells said to start further on: it panicked (SPEC §129).
    let at = 1 + USABLE_PAGE_SIZE;
    let catalog = &mut raw[at..at + USABLE_PAGE_SIZE];
    catalog[9..11].copy_from_slice(&2048u16.to_le_bytes());
    catalog[11..13].copy_from_slice(&9000u16.to_le_bytes());
    catalog[13..].fill(0);
    write("open_raw", "directory_past_the_end", &raw);

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
    wal.extend_from_slice(&trunkdb::fuzzing::wal(&pages, usize::MAX));
    write("recover_wal", "record", &wal);
    // The same batch in two records of a page each (SPEC §91).
    let mut wal = vec![ALL & 0x7F];
    wal.extend_from_slice(&trunkdb::fuzzing::wal(&pages, 1));
    write("recover_wal", "records", &wal);

    // import: an export of the base database.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("seed.trunkdb");
    std::fs::write(&path, trunkdb::fuzzing::seal(base)).unwrap();
    let db = trunkdb::Database::open(&path).unwrap();
    let mut export = Vec::new();
    db.export(&mut export).unwrap();
    write("import", "export.jsonl", &export);

    // queries: programs of 4 to 20 KB, as the smoke run makes them.
    let mut rng: u64 = 0x0105_5EED_0105_5EED;
    for n in 0..16 {
        let len = 4096 + n * 1024;
        let program: Vec<u8> = (0..len)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                rng as u8
            })
            .collect();
        write("queries", &format!("program-{n:02}"), &program);
    }
}
