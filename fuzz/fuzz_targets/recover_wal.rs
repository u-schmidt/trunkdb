//! A valid database next to a WAL: either any bytes, for the WAL's own
//! parsing, or one valid batch of damaged pages, in one record or in
//! several, for recovery writing them back (SPEC §55, §91).
#![no_main]
use libfuzzer_sys::fuzz_target;
use trunkdb::fuzzing::USABLE_PAGE_SIZE;

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
        // In one record, or in several of one to seven pages (SPEC §91).
        let record_pages = match mode & 0x07 {
            0 => usize::MAX,
            pages => pages as usize,
        };
        trunkdb::fuzzing::wal(&images, record_pages)
    };
    trunkdb_fuzz::open_and_exercise(&trunkdb::fuzzing::seal(base), Some(&wal), mode);
});
