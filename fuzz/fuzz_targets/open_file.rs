//! A valid database with bytes changed in its pages, checksums sealed
//! again: every decoder of a page, cell or catalog entry, and every
//! structure built from them (SPEC §55).
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &[u8]| {
    let Some((&mode, edits)) = input.split_first() else {
        return;
    };
    let mut pages = trunkdb_fuzz::base().to_vec();
    trunkdb_fuzz::apply_edits(&mut pages, edits);
    let file = trunkdb::fuzzing::seal(&pages);
    trunkdb_fuzz::open_and_exercise(&file, None, mode);
});
