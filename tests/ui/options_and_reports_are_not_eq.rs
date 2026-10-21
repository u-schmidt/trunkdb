// Options and reports can get a field that isn't `Eq`, a float say, in 1.x, so none
// of them is `Eq` (SPEC §118). One error each.
use trunkdb::query::{ExecutionStats, QueryPlan, SortWork};
use trunkdb::{CheckReport, CollectionStats, Compacted, FileInfo, ImportOptions, IndexInfo};
use trunkdb::{IndexOptions, IndexStats, OpenOptions, PageUse, Recovered, SnapshotInfo, Stats, Summary};

// The calls start past line 9 and the helper is below them: a message
// quoting lines of one and of two digits is laid out differently by Rust
// 1.89 and by stable, which trybuild would see as a mismatch.
#[rustfmt::skip]
fn main() {
    eq::<OpenOptions>(); eq::<ImportOptions>(); eq::<IndexOptions>(); eq::<IndexInfo>();
    eq::<ExecutionStats>(); eq::<SortWork>(); eq::<QueryPlan>(); eq::<Compacted>();
    eq::<SnapshotInfo>(); eq::<PageUse>(); eq::<Recovered>(); eq::<Summary>();
    eq::<CheckReport>(); eq::<FileInfo>(); eq::<Stats>(); eq::<CollectionStats>();
    eq::<IndexStats>();
}

fn eq<T: Eq>() {}
