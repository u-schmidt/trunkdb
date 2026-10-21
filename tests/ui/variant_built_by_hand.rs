// Outside trunkdb a condition is built with its methods (SPEC §117).
use trunkdb::Document;
use trunkdb::query::{Condition, Op};

fn main() {
    #[rustfmt::skip]
    let _ = Condition::Compare { field: "a".to_string(), op: Op::Eq, value: Document::Int(1) };
}
