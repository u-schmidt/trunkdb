// Helpers for trunkdb's own use, not part of its API (SPEC §118).
use trunkdb::Document;
use trunkdb::query::Filter;

#[rustfmt::skip]
fn main() {
    let _ = Document::Null.nests_too_deep();
    let _ = Filter::new().apply_to(Vec::<Document>::new(), |doc| doc);
}
