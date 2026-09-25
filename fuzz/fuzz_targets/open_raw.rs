//! Any bytes as a database file, checksums sealed: the header, and files
//! of any length (SPEC §55).
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &[u8]| {
    let Some((&calls, pages)) = input.split_first() else {
        return;
    };
    let file = trunkdb::fuzzing::seal(pages);
    trunkdb_fuzz::open_and_exercise(&file, None, calls);
});
