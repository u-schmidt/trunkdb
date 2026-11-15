//! trunkdb for Python (SPEC §197): the compiled half of the `trunkdb`
//! package, `trunkdb._trunkdb`, built with PyO3 on the library itself.
//! Documents cross as Python's own types; the GIL is let go around every
//! call into trunkdb.

mod collection;
mod convert;
mod database;
mod errors;
mod live;
mod text;

use pyo3::prelude::*;

/// trunkdb's release, as `"0.41.0"`.
#[pyfunction]
fn version() -> String {
    db::RELEASE.to_string()
}

#[pymodule]
fn _trunkdb(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(database::open, m)?)?;
    m.add_function(wrap_pyfunction!(version, m)?)?;
    m.add_class::<database::PyDatabase>()?;
    m.add_class::<database::Snapshot>()?;
    m.add_class::<database::Cursor>()?;
    m.add_class::<database::Upserted>()?;
    m.add_class::<collection::Collection>()?;
    m.add_class::<collection::View>()?;
    m.add_class::<convert::PyDocId>()?;
    m.add_class::<live::LiveQuery>()?;
    m.add_class::<live::LiveAggregate>()?;
    m.add_class::<live::Changes>()?;
    errors::add(m)?;
    Ok(())
}
