// Options and reports can get a field that isn't `Copy` in 1.x, so none
// of them is (SPEC §118). One error each.
use trunkdb::query::{ExecutionStats, QueryPlan, SortWork};
use trunkdb::{CheckReport, CollectionStats, Compacted, FileInfo, ImportOptions, IndexInfo};
use trunkdb::{IndexOptions, IndexStats, OpenOptions, PageUse, Recovered, SnapshotInfo, Stats, Summary};

// The calls start past line 9 and the helper is below them: a message
// quoting lines of one and of two digits is laid out differently by Rust
// 1.89 and by stable, which trybuild would see as a mismatch.
#[rustfmt::skip]
fn main() {
    copy::<OpenOptions>(); copy::<ImportOptions>(); copy::<IndexOptions>(); copy::<IndexInfo>();
    copy::<ExecutionStats>(); copy::<SortWork>(); copy::<QueryPlan>(); copy::<Compacted>();
    copy::<SnapshotInfo>(); copy::<PageUse>(); copy::<Recovered>(); copy::<Summary>();
    copy::<CheckReport>(); copy::<FileInfo>(); copy::<Stats>(); copy::<CollectionStats>();
    copy::<IndexStats>();
}

fn copy<T: Copy>() {}
