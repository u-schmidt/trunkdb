//! A collection, to read and write, and a snapshot's, to read (SPEC
//! §197.3): a name and its database, nothing to close.

use crate::convert::{self, PyDocId};
use crate::database::{Cursor, PyDatabase, Snapshot, Upserted, quoted, run};
use crate::text;
use db::query::{Aggregate, Filter};
use db::{DocId, Document, IndexInfo};
use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::{PyList, PyString};

/// A collection read as it is now, or as a snapshot holds it.
enum Reader {
    Live(db::Collection<Document>),
    At(db::View<Document>),
}

impl Reader {
    fn get(&self, id: &DocId) -> db::Result<Option<Document>> {
        match self {
            Reader::Live(c) => c.get(id),
            Reader::At(v) => v.get(id),
        }
    }
    fn find(&self, filter: Filter) -> db::Result<Vec<Document>> {
        match self {
            Reader::Live(c) => c.find(filter),
            Reader::At(v) => v.find(filter),
        }
    }
    fn find_one(&self, filter: Filter) -> db::Result<Option<Document>> {
        match self {
            Reader::Live(c) => c.find_one(filter),
            Reader::At(v) => v.find_one(filter),
        }
    }
    fn count(&self, filter: Filter) -> db::Result<usize> {
        match self {
            Reader::Live(c) => c.count(filter),
            Reader::At(v) => v.count(filter),
        }
    }
    fn cursor(&self, filter: Filter) -> db::Result<db::Cursor<Document>> {
        match self {
            Reader::Live(c) => c.cursor(filter),
            Reader::At(v) => v.cursor(filter),
        }
    }
    fn aggregate(&self, filter: Filter, aggregate: &Aggregate) -> db::Result<Vec<Document>> {
        match self {
            Reader::Live(c) => c.aggregate(filter, aggregate),
            Reader::At(v) => v.aggregate(filter, aggregate),
        }
    }
    fn explain(&self, filter: Filter) -> db::Result<db::query::Explained> {
        match self {
            Reader::Live(c) => c.explain_run(filter),
            Reader::At(v) => v.explain_run(filter),
        }
    }
    fn indexes(&self) -> db::Result<Vec<IndexInfo>> {
        match self {
            Reader::Live(c) => c.indexes(),
            Reader::At(v) => v.indexes(),
        }
    }
}

/// `read` of `reader`, the GIL let go while it runs.
fn read<T: Send>(
    py: Python<'_>,
    reader: Reader,
    read: impl FnOnce(&Reader) -> db::Result<T> + Send,
) -> PyResult<T> {
    py.detach(move || read(&reader))
        .map_err(|e| crate::errors::from_db(py, e))
}

/// A filter's text bound to every value of `params`.
fn filter_of(py: Python<'_>, filter: &str, params: Option<&Bound<'_, PyAny>>) -> PyResult<Filter> {
    text::filter(py, filter, &convert::params_in(params)?, false)
}

// The reads `Collection` and `View` share.

fn get(py: Python<'_>, reader: Reader, id: &Bound<'_, PyAny>) -> PyResult<Option<Py<PyAny>>> {
    let id = convert::id_in(id)?;
    let found = read(py, reader, move |r| r.get(&id))?;
    found
        .map(|doc| convert::document_out(py, &doc).map(Bound::unbind))
        .transpose()
}

fn find<'py>(
    py: Python<'py>,
    reader: Reader,
    filter: &str,
    params: Option<&Bound<'_, PyAny>>,
) -> PyResult<Bound<'py, PyList>> {
    let filter = filter_of(py, filter, params)?;
    let found = read(py, reader, move |r| r.find(filter))?;
    convert::documents_out(py, &found)
}

fn find_one(
    py: Python<'_>,
    reader: Reader,
    filter: &str,
    params: Option<&Bound<'_, PyAny>>,
) -> PyResult<Option<Py<PyAny>>> {
    let filter = filter_of(py, filter, params)?;
    let found = read(py, reader, move |r| r.find_one(filter))?;
    found
        .map(|doc| convert::document_out(py, &doc).map(Bound::unbind))
        .transpose()
}

fn count(
    py: Python<'_>,
    reader: Reader,
    filter: &str,
    params: Option<&Bound<'_, PyAny>>,
) -> PyResult<usize> {
    let filter = filter_of(py, filter, params)?;
    read(py, reader, move |r| r.count(filter))
}

fn cursor(
    py: Python<'_>,
    db: Py<PyDatabase>,
    reader: Reader,
    filter: &str,
    params: Option<&Bound<'_, PyAny>>,
) -> PyResult<Cursor> {
    let filter = filter_of(py, filter, params)?;
    let opened = read(py, reader, move |r| r.cursor(filter))?;
    Cursor::new(py, db, opened)
}

fn aggregate<'py>(
    py: Python<'py>,
    reader: Reader,
    filter: &str,
    aggregate: &str,
    params: Option<&Bound<'_, PyAny>>,
) -> PyResult<Bound<'py, PyList>> {
    let params = convert::params_in(params)?;
    let (filter, aggregate) = text::aggregate_args(py, filter, aggregate, &params)?;
    let rows = read(py, reader, move |r| r.aggregate(filter, &aggregate))?;
    convert::documents_out(py, &rows)
}

fn explain(
    py: Python<'_>,
    reader: Reader,
    filter: &str,
    params: Option<&Bound<'_, PyAny>>,
) -> PyResult<String> {
    let filter = filter_of(py, filter, params)?;
    read(py, reader, move |r| {
        r.explain(filter).map(|e| e.to_string())
    })
}

fn indexes<'py>(py: Python<'py>, reader: Reader) -> PyResult<Bound<'py, PyList>> {
    let found = read(py, reader, |r| r.indexes())?;
    let docs: Vec<Document> = found.iter().map(IndexInfo::to_document).collect();
    convert::documents_out(py, &docs)
}

/// An index's fields: a name, or a list of names.
fn index_fields(fields: &Bound<'_, PyAny>) -> PyResult<Vec<String>> {
    if let Ok(name) = fields.cast::<PyString>() {
        return Ok(vec![name.to_str()?.to_string()]);
    }
    // A str was taken above, so this is a list of names, not letters.
    if let Ok(names) = fields.extract::<Vec<String>>() {
        return Ok(names);
    }
    Err(PyTypeError::new_err(
        "an index's fields are a name, or a list of names, like [\"team\", \"email\"]",
    ))
}

/// A collection of a database, to read and write: from
/// `Database.collection(name)`.
#[pyclass(name = "Collection", module = "trunkdb", frozen)]
pub(crate) struct Collection {
    db: Py<PyDatabase>,
    name: String,
}

impl Collection {
    pub(crate) fn new(db: Py<PyDatabase>, name: String) -> Collection {
        Collection { db, name }
    }

    fn state(&self) -> &crate::database::DbState {
        &self.db.get().state
    }

    fn reader(&self) -> PyResult<Reader> {
        Ok(Reader::Live(self.state().get()?.collection(&self.name)))
    }

    /// `call` on this collection, the GIL let go while it runs.
    fn write<T: Send>(
        &self,
        py: Python<'_>,
        call: impl FnOnce(&db::Collection<Document>) -> db::Result<T> + Send,
    ) -> PyResult<T> {
        let name = &self.name;
        run(py, self.state(), move |db| call(&db.collection(name)))
    }
}

#[pymethods]
impl Collection {
    /// The collection's name.
    #[getter]
    fn name(&self) -> &str {
        &self.name
    }

    /// The document with `id`, a `DocId` or its text; `None` if there is
    /// none.
    fn get(&self, py: Python<'_>, id: &Bound<'_, PyAny>) -> PyResult<Option<Py<PyAny>>> {
        get(py, self.reader()?, id)
    }

    /// What `filter` finds, as a list; `""` is every document.
    #[pyo3(signature = (filter = "", params = None))]
    fn find<'py>(
        &self,
        py: Python<'py>,
        filter: &str,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Bound<'py, PyList>> {
        find(py, self.reader()?, filter, params)
    }

    /// The first document `filter` finds; `None` if none.
    #[pyo3(signature = (filter = "", params = None))]
    fn find_one(
        &self,
        py: Python<'_>,
        filter: &str,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Option<Py<PyAny>>> {
        find_one(py, self.reader()?, filter, params)
    }

    /// How many documents `filter` finds.
    #[pyo3(signature = (filter = "", params = None))]
    fn count(
        &self,
        py: Python<'_>,
        filter: &str,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<usize> {
        count(py, self.reader()?, filter, params)
    }

    /// What `filter` finds, handed out a document at a time, of the
    /// collection as it is now.
    #[pyo3(signature = (filter = "", params = None))]
    fn cursor(
        &self,
        py: Python<'_>,
        filter: &str,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Cursor> {
        cursor(py, self.db.clone_ref(py), self.reader()?, filter, params)
    }

    /// An aggregate's rows over what `filter` finds: `aggregate` as text,
    /// `"count() as n group by team"`. `params` holds the values of both
    /// texts' parameters, each taking its own.
    #[pyo3(signature = (filter, aggregate, params = None))]
    fn aggregate<'py>(
        &self,
        py: Python<'py>,
        filter: &str,
        aggregate: &str,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Bound<'py, PyList>> {
        self::aggregate(py, self.reader()?, filter, aggregate, params)
    }

    /// Runs `filter` and says how: its plan, what it read and how much.
    #[pyo3(signature = (filter = "", params = None))]
    fn explain(
        &self,
        py: Python<'_>,
        filter: &str,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<String> {
        explain(py, self.reader()?, filter, params)
    }

    /// Inserts `doc`, a dict; its id, the one its `_id` gives or a new one.
    fn insert(&self, py: Python<'_>, doc: &Bound<'_, PyAny>) -> PyResult<PyDocId> {
        let doc = convert::document_in(doc, "")?;
        self.write(py, move |c| c.insert(doc)).map(PyDocId)
    }

    /// Inserts each of `docs`, in one batch: all of them or none. Their
    /// ids, in order.
    fn insert_many(&self, py: Python<'_>, docs: &Bound<'_, PyAny>) -> PyResult<Vec<PyDocId>> {
        let mut all = Vec::new();
        for (at, doc) in docs.try_iter()?.enumerate() {
            all.push(convert::document_in(&doc?, &format!("[{at}]"))?);
        }
        if all.is_empty() {
            return Ok(Vec::new());
        }
        let name = &self.name;
        let ids = run(py, self.state(), move |db| {
            let collection = db.collection::<Document>(name);
            let mut batch = db.batch();
            let ids = (all.into_iter())
                .map(|doc| batch.insert(&collection, doc))
                .collect::<db::Result<Vec<_>>>()?;
            batch.commit()?;
            Ok(ids)
        })?;
        Ok(ids.into_iter().map(PyDocId).collect())
    }

    /// Replaces the document with `id` by `doc`, whole; whether there was
    /// one. None is inserted.
    fn update(
        &self,
        py: Python<'_>,
        id: &Bound<'_, PyAny>,
        doc: &Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        let id = convert::id_in(id)?;
        let doc = convert::document_in(doc, "")?;
        self.write(py, move |c| c.update(&id, doc))
    }

    /// Makes `update`, MongoDB's operators as a dict, `{"$inc": {"n": 1}}`,
    /// to what `filter` finds; how many documents it changed.
    #[pyo3(signature = (filter, update, params = None))]
    fn update_many(
        &self,
        py: Python<'_>,
        filter: &str,
        update: &Bound<'_, PyAny>,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<usize> {
        let filter = filter_of(py, filter, params)?;
        let update = text::update(update)?;
        self.write(py, move |c| c.update_fields(filter, &update))
    }

    /// Deletes the document with `id`; whether there was one.
    fn delete(&self, py: Python<'_>, id: &Bound<'_, PyAny>) -> PyResult<bool> {
        let id = convert::id_in(id)?;
        self.write(py, move |c| c.delete(&id))
    }

    /// Deletes what `filter` finds; how many. `""` is every document.
    #[pyo3(signature = (filter, params = None))]
    fn delete_many(
        &self,
        py: Python<'_>,
        filter: &str,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<usize> {
        let filter = filter_of(py, filter, params)?;
        self.write(py, move |c| c.delete_many(filter))
    }

    /// Makes `update` to the one document `filter` finds, or inserts one
    /// made of the filter's `=`s and then `update`. Several matches fail.
    #[pyo3(signature = (filter, update, params = None))]
    fn upsert(
        &self,
        py: Python<'_>,
        filter: &str,
        update: &Bound<'_, PyAny>,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Upserted> {
        let filter = filter_of(py, filter, params)?;
        let update = text::update(update)?;
        let done = self.write(py, move |c| c.upsert_fields(filter, &update))?;
        Ok(Upserted {
            id: PyDocId(done.id()),
            inserted: matches!(done, db::Upserted::Inserted(_)),
        })
    }

    /// A live query of `filter`: first an `Entered` for each document it
    /// finds, then, as each commit commits, what came to match it
    /// (`Entered`), what no longer does (`Left`), and what was written
    /// that still does (`Changed`). With an `order by`, and a `limit` or
    /// none, each event says where in the list.
    #[pyo3(signature = (filter = "", params = None))]
    fn watch(
        &self,
        py: Python<'_>,
        filter: &str,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<crate::live::LiveQuery> {
        let filter = filter_of(py, filter, params)?;
        let query = self.write(py, move |c| c.watch(filter))?;
        Ok(crate::live::LiveQuery::new(
            crate::live::Source::Query(query),
            Some(self.name.clone()),
        ))
    }

    /// A live aggregate: first a `RowEntered` for each row of
    /// `aggregate` over what `filter` finds, then, as each commit to the
    /// collection commits, the rows that came (`RowEntered`), left
    /// (`RowLeft`), or changed (`RowChanged`). `params` as `aggregate`'s.
    #[pyo3(signature = (filter, aggregate, params = None))]
    fn watch_aggregate(
        &self,
        py: Python<'_>,
        filter: &str,
        aggregate: &str,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<crate::live::LiveAggregate> {
        let params = convert::params_in(params)?;
        let (filter, aggregate) = text::aggregate_args(py, filter, aggregate, &params)?;
        let live = self.write(py, move |c| c.watch_aggregate(filter, &aggregate))?;
        Ok(crate::live::LiveAggregate::new(
            crate::live::Source::Aggregate(live),
            Some(self.name.clone()),
        ))
    }

    /// Makes an index on `fields`, a name or a list of names, unless there
    /// is one; whether this call made it.
    #[pyo3(signature = (fields, *, unique = false, sparse = false))]
    fn ensure_index(
        &self,
        py: Python<'_>,
        fields: &Bound<'_, PyAny>,
        unique: bool,
        sparse: bool,
    ) -> PyResult<bool> {
        let fields = index_fields(fields)?;
        let mut options = db::IndexOptions::new();
        if unique {
            options = options.unique();
        }
        if sparse {
            options = options.sparse();
        }
        self.write(py, move |c| c.ensure_index_with(fields, options))
    }

    /// Drops the index on `fields`; whether there was one.
    fn drop_index(&self, py: Python<'_>, fields: &Bound<'_, PyAny>) -> PyResult<bool> {
        let fields = index_fields(fields)?;
        self.write(py, move |c| c.drop_index(fields))
    }

    /// The collection's indexes: `{"fields": [...], "unique": ...,
    /// "sparse": ...}` each.
    fn indexes<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        indexes(py, self.reader()?)
    }

    /// Drops the collection, its documents and indexes; whether it was
    /// there.
    fn drop(&self, py: Python<'_>) -> PyResult<bool> {
        let name = &self.name;
        run(py, self.state(), move |db| db.drop_collection(name))
    }

    fn __repr__(&self, py: Python<'_>) -> String {
        format!("<trunkdb.Collection {}>", quoted(py, &self.name))
    }
}

/// A collection as a snapshot holds it, to read: from
/// `Snapshot.collection(name)`.
#[pyclass(name = "View", module = "trunkdb", frozen)]
pub(crate) struct View {
    snapshot: Py<Snapshot>,
    name: String,
}

impl View {
    pub(crate) fn new(snapshot: Py<Snapshot>, name: String) -> View {
        View { snapshot, name }
    }

    fn reader(&self) -> PyResult<Reader> {
        Ok(Reader::At(
            self.snapshot.get().get()?.collection(&self.name),
        ))
    }
}

#[pymethods]
impl View {
    /// The collection's name.
    #[getter]
    fn name(&self) -> &str {
        &self.name
    }

    /// The document with `id`, as the snapshot holds it; `None` if none.
    fn get(&self, py: Python<'_>, id: &Bound<'_, PyAny>) -> PyResult<Option<Py<PyAny>>> {
        get(py, self.reader()?, id)
    }

    /// What `filter` finds, as a list.
    #[pyo3(signature = (filter = "", params = None))]
    fn find<'py>(
        &self,
        py: Python<'py>,
        filter: &str,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Bound<'py, PyList>> {
        find(py, self.reader()?, filter, params)
    }

    /// The first document `filter` finds; `None` if none.
    #[pyo3(signature = (filter = "", params = None))]
    fn find_one(
        &self,
        py: Python<'_>,
        filter: &str,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Option<Py<PyAny>>> {
        find_one(py, self.reader()?, filter, params)
    }

    /// How many documents `filter` finds.
    #[pyo3(signature = (filter = "", params = None))]
    fn count(
        &self,
        py: Python<'_>,
        filter: &str,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<usize> {
        count(py, self.reader()?, filter, params)
    }

    /// What `filter` finds, handed out a document at a time.
    #[pyo3(signature = (filter = "", params = None))]
    fn cursor(
        &self,
        py: Python<'_>,
        filter: &str,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Cursor> {
        let db = self.snapshot.get().db.clone_ref(py);
        cursor(py, db, self.reader()?, filter, params)
    }

    /// An aggregate's rows over what `filter` finds.
    #[pyo3(signature = (filter, aggregate, params = None))]
    fn aggregate<'py>(
        &self,
        py: Python<'py>,
        filter: &str,
        aggregate: &str,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Bound<'py, PyList>> {
        self::aggregate(py, self.reader()?, filter, aggregate, params)
    }

    /// Runs `filter` and says how.
    #[pyo3(signature = (filter = "", params = None))]
    fn explain(
        &self,
        py: Python<'_>,
        filter: &str,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<String> {
        explain(py, self.reader()?, filter, params)
    }

    /// The collection's indexes, as the snapshot holds them.
    fn indexes<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        indexes(py, self.reader()?)
    }

    fn __repr__(&self, py: Python<'_>) -> String {
        format!("<trunkdb.View {}>", quoted(py, &self.name))
    }
}
