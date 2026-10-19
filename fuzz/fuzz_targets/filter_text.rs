//! Any text as a filter (SPEC §111). Reading it never panics, and a
//! filter it reads prints as text that reads back as the same filter.
#![no_main]
use libfuzzer_sys::fuzz_target;
use trunkdb::query::Filter;

fuzz_target!(|input: &[u8]| {
    let Ok(text) = std::str::from_utf8(input) else {
        return;
    };
    if let Ok(filter) = text.parse::<Filter>() {
        let printed = filter.to_string();
        let again: Filter = printed
            .parse()
            .unwrap_or_else(|e| panic!("{text:?} printed as {printed:?}: {e}"));
        assert_eq!(again.to_string(), printed, "{text:?}");
    }
});
