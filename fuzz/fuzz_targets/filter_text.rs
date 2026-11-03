//! Any text as a filter (SPEC §111), and as a template of one (§163).
//! Reading never panics; a filter read prints as text that reads back
//! as the same filter; a template binds, each parameter to a value that
//! fits where it stands, and the filter it binds to prints and reads
//! back the same too; a text that reads as a filter binds, without
//! parameters, to that filter.
#![no_main]
use libfuzzer_sys::fuzz_target;
use trunkdb::Document;
use trunkdb::query::{BindError, Filter, Params, Template};

fn reads_back(text: &str, filter: &Filter) {
    let printed = filter.to_string();
    let again: Filter = printed
        .parse()
        .unwrap_or_else(|e| panic!("{text:?} printed as {printed:?}: {e}"));
    assert_eq!(again.to_string(), printed, "{text:?}");
}

fuzz_target!(|input: &[u8]| {
    let Ok(text) = std::str::from_utf8(input) else {
        return;
    };
    let plain = text.parse::<Filter>();
    if let Ok(filter) = &plain {
        reads_back(text, filter);
    }
    let Ok(template) = text.parse::<Template<Filter>>() else {
        assert!(
            plain.is_err(),
            "{text:?} reads as a filter, not as a template"
        );
        return;
    };
    assert_eq!(template.to_string(), text);
    // A count fits anywhere but in `in :list`, which takes an array.
    let mut params = Params::new();
    for name in template.params() {
        params = params.set(name, 1);
    }
    let bound = loop {
        match template.bind(&params) {
            Ok(bound) => break bound,
            Err(BindError::NotAnArray { name, .. }) => {
                params = params.set(name, Document::Array(vec![Document::Int(1)]));
            }
            Err(e) => panic!("{text:?}: {e}"),
        }
    };
    reads_back(text, &bound);
    if let Ok(filter) = &plain {
        assert!(template.params().is_empty(), "{text:?}");
        assert_eq!(bound.to_string(), filter.to_string(), "{text:?}");
    }
});
