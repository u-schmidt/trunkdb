//! Any bytes as a program of writes and reads on a valid database, every
//! read held against the same filter applied in memory (SPEC §105).
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &[u8]| {
    trunkdb_fuzz::queries::run(input);
});
