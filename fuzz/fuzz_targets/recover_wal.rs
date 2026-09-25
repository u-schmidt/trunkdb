//! A valid database next to a WAL: either any bytes, for the WAL's own
//! parsing, or one valid record of damaged pages, for recovery writing
//! them back (SPEC §55).
#![no_main]
use libfuzzer_sys::fuzz_target;
use trunkdb::storage::USABLE_PAGE_SIZE;

fuzz_target!(|input: &[u8]| {
    let Some((&mode, rest)) = input.split_first() else {
        return;
    };
    let base = trunkdb_fuzz::base();
    let wal = if mode & 0x80 == 0 {
        rest.to_vec()
    } else {
        let mut pages = base.to_vec();
        let changed = trunkdb_fuzz::apply_edits(&mut pages, rest);
        let images: Vec<(u64, Vec<u8>)> = changed
            .into_iter()
            .map(|page| {
                let at = page * USABLE_PAGE_SIZE;
                (page as u64, pages[at..at + USABLE_PAGE_SIZE].to_vec())
            })
            .collect();
        trunkdb::fuzzing::wal(&images)
    };
    trunkdb_fuzz::open_and_exercise(&trunkdb::fuzzing::seal(base), Some(&wal), mode);
});
