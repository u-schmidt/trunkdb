//! Any bytes as a JSON Lines export into a fresh database. An import that
//! succeeds must leave a database `check` finds nothing wrong with (SPEC
//! §55).
#![no_main]
use libfuzzer_sys::fuzz_target;
use trunkdb::Database;

fuzz_target!(|input: &[u8]| {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("fuzz.trunkdb")).unwrap();
    if db.import(input).is_ok() {
        let report = db.check().unwrap();
        assert!(
            report.is_ok(),
            "an import that succeeded fails check: {report:?}"
        );
        trunkdb_fuzz::exercise(&db, trunkdb_fuzz::FIND | trunkdb_fuzz::EXPORT);
    }
});
