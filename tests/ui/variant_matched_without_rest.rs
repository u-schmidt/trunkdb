// Outside trunkdb a variant with named fields is matched with `..`,
// so a field added to it breaks nothing (SPEC §117). One error each.
use trunkdb::query::{Condition, QueryPlan};

#[rustfmt::skip]
fn main() {
    let condition = Condition::exists("a");
    if let Condition::Compare { field, op, value } = &condition { let _ = (field, op, value); }
    if let Condition::Exists { field } = &condition { let _ = field; }
    if let Condition::Size { field, op, size } = &condition { let _ = (field, op, size); }
    if let Condition::ElemMatch { field, condition } = &condition { let _ = (field, condition); }
    let plan = QueryPlan::Scan;
    if let QueryPlan::Index { field } = &plan { let _ = field; }
    if let QueryPlan::IndexOrder { field } = &plan { let _ = field; }
    if let QueryPlan::IndexUnion { fields } = &plan { let _ = fields; }
}
