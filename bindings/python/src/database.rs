//! The database, its snapshots and cursors (SPEC §197.3, §197.6): each
//! closed by `close()`, by leaving its `with`, or, with a
//! `ResourceWarning`, when Python frees it; a snapshot and a cursor also
//! when their database closes.

use crate::collection::{Collection, View};
use crate::convert::{self, PyDocId};
use crate::errors::{self, Parse};
use db::{BatchLineError, BatchLines, Database, Document};
use pyo3::exceptions::{PyResourceWarning, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyList, PyString};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

/// A lock, whatever a panic left in it: every value here is whole between
/// two statements.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a database closes when it closes.
pub(crate) trait Closes: Send + Sync {
    fn close(&self);
}

/// A value until it is closed.
pub(crate) struct Slot<T>(Mutex<Option<T>>);

impl<T> Slot<T> {
    pub(crate) fn new(value: T) -> Slot<T> {
        Slot(Mutex::new(Some(value)))
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Option<T>> {
        lock(&self.0)
    }

    pub(crate) fn is_open(&self) -> bool {
        self.lock().is_some()
    }

    /// The value, dropped: what it held is let go.
    pub(crate) fn take(&self) -> Option<T> {
        self.lock().take()
    }
}

impl<T: Send> Closes for Slot<T> {
    fn close(&self) {
        drop(self.take());
    }
}

/// An open database and what it closes with it.
pub(crate) struct DbState {
    db: Slot<Database>,
    children: Mutex<Vec<Weak<dyn Closes>>>,
    path: String,
}

impl DbState {
    /// The database, for one call: a call under way keeps the file open
    /// past a `close` until it returns.
    pub(crate) fn get(&self) -> PyResult<Database> {
        self.db
            .lock()
            .clone()
            .ok_or_else(|| errors::closed("database"))
    }

    /// `child` closed with the database; closed now if the database
    /// closed while it was made. A `close` takes the database before its
    /// children, so, with the children locked, a database still open
    /// will close `child` too.
    pub(crate) fn adopt<C: Closes + 'static>(
        &self,
        py: Python<'_>,
        child: &Arc<C>,
    ) -> PyResult<()> {
        {
            let mut children = lock(&self.children);
            if self.db.is_open() {
                children.retain(|c| c.strong_count() > 0);
                children.push(Arc::downgrade(child) as Weak<dyn Closes>);
                return Ok(());
            }
        }
        py.detach(|| child.close());
        Err(errors::closed("database"))
    }

    fn close(&self, py: Python<'_>) {
        let db = self.db.take();
        let children = std::mem::take(&mut *lock(&self.children));
        for child in children.iter().filter_map(Weak::upgrade) {
            child.close();
        }
        // Without the GIL: the last of the database writes what its log
        // holds back into the file.
        py.detach(move || drop(db));
    }
}

/// `call` on the database, the GIL let go while it runs.
pub(crate) fn run<T: Send>(
    py: Python<'_>,
    state: &DbState,
    call: impl FnOnce(&Database) -> db::Result<T> + Send,
) -> PyResult<T> {
    let db = state.get()?;
    py.detach(move || call(&db))
        .map_err(|e| errors::from_db(py, e))
}

/// `text` as Python's `repr` writes it.
pub(crate) fn quoted(py: Python<'_>, text: &str) -> String {
    PyString::new(py, text)
        .repr()
        .map(|r| r.to_string())
        .unwrap_or_else(|_| format!("{text:?}"))
}

/// A `ResourceWarning` that `what` was never closed, as for a file.
///
/// Python frees an object while an exception is on its way too, as a
/// cursor of a `for` the exception leaves: that exception is put aside
/// for the warning and back after, or the warning would take its place.
pub(crate) fn warn_unclosed(py: Python<'_>, what: &str) {
    let pending = PyErr::take(py);
    let message = std::ffi::CString::new(format!("unclosed {what}")).unwrap_or_default();
    if let Err(e) = PyErr::warn(
        py,
        &py.get_type::<PyResourceWarning>().into_any(),
        &message,
        1,
    ) {
        e.write_unraisable(py, None);
    }
    if let Some(pending) = pending {
        pending.restore(py);
    }
}

/// An open trunkdb database: from `trunkdb.open`. One for the program,
/// from any thread.
#[pyclass(name = "Database", module = "trunkdb", frozen)]
pub(crate) struct PyDatabase {
    pub(crate) state: Arc<DbState>,
}

/// Opens the database at `path`, creating it if it isn't there.
#[pyfunction]
#[pyo3(signature = (
    path,
    *,
    read_only = false,
    cache_size = None,
    snapshot_memory = None,
    checkpoint_pages = None,
    checkpoint_wal_bytes = None,
    watch_every = None,
))]
// One argument for each of Python's keyword options.
#[allow(clippy::too_many_arguments)]
pub(crate) fn open(
    py: Python<'_>,
    path: PathBuf,
    read_only: bool,
    cache_size: Option<usize>,
    snapshot_memory: Option<usize>,
    checkpoint_pages: Option<usize>,
    checkpoint_wal_bytes: Option<u64>,
    watch_every: Option<f64>,
) -> PyResult<PyDatabase> {
    let mut options = db::OpenOptions::default().read_only(read_only);
    if let Some(n) = cache_size {
        options = options.cache_size(n);
    }
    if let Some(n) = snapshot_memory {
        options = options.snapshot_memory(n);
    }
    if let Some(n) = checkpoint_pages {
        options = options.checkpoint_pages(n);
    }
    if let Some(n) = checkpoint_wal_bytes {
        options = options.checkpoint_wal_bytes(n);
    }
    if let Some(every) = watch_every {
        if !(every.is_finite() && every > 0.0 && every <= 86_400.0) {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "watch_every is a number of seconds, more than 0, at most a day",
            ));
        }
        options = options.watch_every(std::time::Duration::from_secs_f64(every));
    }
    let shown = path.display().to_string();
    let db = py
        .detach(|| Database::open_with(&path, options))
        .map_err(|e| match e {
            db::Error::Io(e) => errors::io(py, e, Some(&shown)),
            e => errors::from_db(py, e),
        })?;
    Ok(PyDatabase {
        state: Arc::new(DbState {
            db: Slot::new(db),
            children: Mutex::new(Vec::new()),
            path: shown,
        }),
    })
}

/// A report handed to Python as a dict.
fn report(py: Python<'_>, doc: Document) -> PyResult<Py<PyAny>> {
    Ok(convert::document_out(py, &doc)?.unbind())
}

/// A file system error about `path`, else the error's own.
fn about(py: Python<'_>, path: &str, error: db::Error) -> PyErr {
    match error {
        db::Error::Io(e) => errors::io(py, e, Some(path)),
        e => errors::from_db(py, e),
    }
}

#[pymethods]
impl PyDatabase {
    /// The collection `name`: costs nothing, holds nothing to close.
    fn collection(slf: &Bound<'_, Self>, name: String) -> Collection {
        Collection::new(slf.clone().unbind(), name)
    }

    /// The collections' names, in name order.
    fn collections(&self, py: Python<'_>) -> PyResult<Vec<String>> {
        run(py, &self.state, |db| db.collections())
    }

    /// Drops a collection, its documents and indexes; whether it was there.
    fn drop_collection(&self, py: Python<'_>, name: String) -> PyResult<bool> {
        run(py, &self.state, move |db| db.drop_collection(&name))
    }

    /// A `Commit` for each batch committed from now on, with what it
    /// inserted, updated and deleted, and the collections it dropped.
    fn subscribe(&self, py: Python<'_>) -> PyResult<crate::live::Changes> {
        let changes = run(py, &self.state, |db| db.subscribe())?;
        Ok(crate::live::Changes::new(
            crate::live::Source::Changes(changes),
            None,
        ))
    }

    /// The database as it is now, kept for reads however it is written to
    /// after. Close it soon: while open, it keeps what was written since in
    /// memory.
    fn snapshot(slf: &Bound<'_, Self>) -> PyResult<Snapshot> {
        let py = slf.py();
        let state = &slf.get().state;
        let taken = run(py, state, |db| db.snapshot())?;
        let slot = Arc::new(Slot::new(taken));
        state.adopt(py, &slot)?;
        Ok(Snapshot {
            db: slf.clone().unbind(),
            slot,
        })
    }

    /// Runs a batch of ops, all committed or none; with `dry_run`, each run
    /// and rolled back. `ops` is a list of op dicts, or the JSON lines
    /// `trunkdb batch` reads; `params` holds the values for every op's
    /// filter. Returns `{"ops": [...], "total": ..., "dry_run": ...}`.
    #[pyo3(signature = (ops, params = None, *, dry_run = false))]
    fn batch(
        &self,
        py: Python<'_>,
        ops: &Bound<'_, PyAny>,
        params: Option<&Bound<'_, PyAny>>,
        dry_run: bool,
    ) -> PyResult<Py<PyAny>> {
        let lines = batch_lines(ops)?;
        let params = convert::params_in(params)?;
        let db = self.state.get()?;
        let ops = BatchLines::read(&lines, &params).map_err(|e| line_error(py, e))?;
        let (batch, ids) = ops.batch(&db).map_err(|e| line_error(py, e))?;
        let outcome = py.detach(|| match dry_run {
            true => batch.dry_run(),
            false => batch.commit(),
        });
        let done = outcome.map_err(|e| {
            let line = ops.failed_line(&db, &e);
            let error = errors::from_db(py, e);
            if let Some(line) = line {
                let _ = error.value(py).setattr("line", line);
            }
            error
        })?;
        let mut written = ops.report(&done, &ids, dry_run);
        ids_as_ids(&mut written);
        report(py, written)
    }

    /// Writes what the log holds into the file, and empties the log.
    fn checkpoint(&self, py: Python<'_>) -> PyResult<()> {
        run(py, &self.state, |db| db.checkpoint())
    }

    /// Rebuilds the file without its free pages: `{"pages_before": ...,
    /// "pages_after": ...}`. Fails while a snapshot is open.
    fn compact(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let done = run(py, &self.state, |db| db.compact())?;
        report(py, done.to_document())
    }

    /// Reads the whole file and checks it: `{"ok": ..., "collections": ...,
    /// "documents": ..., "pages": ..., "problems": [...]}`.
    fn check(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let done = run(py, &self.state, |db| db.check())?;
        report(py, done.to_document())
    }

    /// Where the file's space goes: the file, its pages by use, and each
    /// collection's documents, sizes and indexes.
    fn stats(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let done = run(py, &self.state, |db| db.stats())?;
        report(py, done.to_document())
    }

    /// Exports every collection to the file at `path` as JSON lines,
    /// written beside it and renamed over it once complete.
    fn export(&self, py: Python<'_>, path: PathBuf) -> PyResult<Py<PyAny>> {
        let db = self.state.get()?;
        let shown = path.display().to_string();
        let done = py
            .detach(|| db.export_to_file(&path))
            .map_err(|e| about(py, &shown, e))?;
        report(py, done.to_document())
    }

    /// Imports an export from the file at `path`, as one batch.
    #[pyo3(signature = (path, *, max_line_bytes = None))]
    fn import_file(
        &self,
        py: Python<'_>,
        path: PathBuf,
        max_line_bytes: Option<usize>,
    ) -> PyResult<Py<PyAny>> {
        let db = self.state.get()?;
        let shown = path.display().to_string();
        let mut options = db::ImportOptions::default();
        if let Some(n) = max_line_bytes {
            options = options.max_line_bytes(n);
        }
        let done = py
            .detach(|| {
                let file = std::fs::File::open(&path).map_err(db::Error::Io)?;
                db.import_with(std::io::BufReader::new(file), options)
            })
            .map_err(|e| about(py, &shown, e))?;
        report(py, done.to_document())
    }

    /// Closes the database once every call under way has returned, and its
    /// snapshots and cursors with it. A second close does nothing.
    fn close(&self, py: Python<'_>) {
        self.state.close(py);
    }

    /// Whether `close` has been called.
    #[getter]
    fn closed(&self) -> bool {
        !self.state.db.is_open()
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    #[pyo3(signature = (*_args))]
    fn __exit__(&self, py: Python<'_>, _args: &Bound<'_, pyo3::types::PyTuple>) -> bool {
        self.state.close(py);
        false
    }

    fn __repr__(&self, py: Python<'_>) -> String {
        let closed = if self.closed() { " closed" } else { "" };
        format!(
            "<trunkdb.Database{closed} {}>",
            quoted(py, &self.state.path)
        )
    }
}

impl Drop for PyDatabase {
    fn drop(&mut self) {
        if self.state.db.is_open() {
            Python::attach(|py| {
                warn_unclosed(py, &format!("database {:?}", self.state.path));
                self.state.close(py);
            });
        }
    }
}

/// A batch's ops as the lines `BatchLines::read` reads: a str as it is,
/// a list's dicts each a line.
fn batch_lines(ops: &Bound<'_, PyAny>) -> PyResult<String> {
    if let Ok(text) = ops.cast::<PyString>() {
        return Ok(text.to_str()?.to_string());
    }
    let Ok(list) = ops.cast::<PyList>() else {
        return Err(PyTypeError::new_err(
            "ops is a list of op dicts, or their JSON lines as a str",
        ));
    };
    let mut lines = Vec::with_capacity(list.len());
    for (at, op) in list.iter().enumerate() {
        lines.push(convert::document_in(&op, &format!("ops[{at}]"))?.to_string());
    }
    Ok(lines.join("\n"))
}

/// A batch's line that isn't an op, or an op it refused, its line named.
fn line_error(py: Python<'_>, error: BatchLineError) -> PyErr {
    let line = error.line();
    let message = error.to_string();
    let mut parse = match error {
        BatchLineError::Refused { error, .. } => {
            let error = errors::from_db(py, error);
            if let Some(line) = line {
                let _ = error.value(py).setattr("line", line);
            }
            return error;
        }
        BatchLineError::Filter { text, error, .. } => {
            let mut parse = Parse::new("batch", &message);
            parse.text = Some(text);
            parse.offset = Some(error.offset());
            parse
        }
        BatchLineError::Bind { text, error, .. } => {
            let mut parse = Parse::new("batch", &message);
            parse.text = Some(text);
            parse.offset = error.offset();
            parse.parameter = Some(error.name().to_string());
            parse
        }
        BatchLineError::Unused { name, .. } => {
            let mut parse = Parse::new("batch", &message);
            parse.parameter = Some(name);
            parse
        }
        _ => Parse::new("batch", &message),
    };
    parse.line = line;
    parse.into_err(py)
}

/// A batch's report with each op's id an id, not its text (SPEC §198.4).
fn ids_as_ids(written: &mut Document) {
    let Document::Object(report) = written else {
        return;
    };
    let Some(Document::Array(ops)) = report.get_mut("ops") else {
        return;
    };
    for op in ops {
        if let Document::Object(op) = op
            && let Some(id) = op.get_mut("id")
            && let Document::String(text) = id
            && let Ok(parsed) = text.parse()
        {
            *id = Document::Id(parsed);
        }
    }
}

/// The database as it was at one commit: from `Database.snapshot()`.
#[pyclass(name = "Snapshot", module = "trunkdb", frozen)]
pub(crate) struct Snapshot {
    pub(crate) db: Py<PyDatabase>,
    slot: Arc<Slot<db::Snapshot>>,
}

impl Snapshot {
    /// The snapshot, for one call.
    pub(crate) fn get(&self) -> PyResult<db::Snapshot> {
        self.db.get().state.get()?;
        self.slot
            .lock()
            .clone()
            .ok_or_else(|| errors::closed("snapshot"))
    }
}

#[pymethods]
impl Snapshot {
    /// The collection `name` as the snapshot holds it, to read.
    fn collection(slf: &Bound<'_, Self>, name: String) -> View {
        View::new(slf.clone().unbind(), name)
    }

    /// The collections' names, as the snapshot holds them.
    fn collections(&self, py: Python<'_>) -> PyResult<Vec<String>> {
        let snapshot = self.get()?;
        py.detach(|| snapshot.collections())
            .map_err(|e| errors::from_db(py, e))
    }

    /// Lets go of what the snapshot keeps. A second close does nothing.
    fn close(&self) {
        self.slot.close();
    }

    /// Whether it was closed, or its database.
    #[getter]
    fn closed(&self) -> bool {
        !self.slot.is_open()
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    #[pyo3(signature = (*_args))]
    fn __exit__(&self, _args: &Bound<'_, pyo3::types::PyTuple>) -> bool {
        self.slot.close();
        false
    }

    fn __repr__(&self) -> String {
        let closed = if self.closed() { " closed" } else { "" };
        format!("<trunkdb.Snapshot{closed}>")
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        if self.slot.is_open() {
            Python::attach(|py| warn_unclosed(py, "snapshot"));
            self.slot.close();
        }
    }
}

/// A read handed out a document at a time, of the database as it was
/// when opened: an iterator, and a context manager that closes it. It
/// closes itself at its end.
#[pyclass(name = "Cursor", module = "trunkdb", frozen)]
pub(crate) struct Cursor {
    db: Py<PyDatabase>,
    slot: Arc<Slot<db::Cursor<Document>>>,
    /// Closed by its end, not by `close`: a `next` after it is the end
    /// again, as for any iterator.
    ended: AtomicBool,
}

impl Cursor {
    pub(crate) fn new(
        py: Python<'_>,
        db: Py<PyDatabase>,
        cursor: db::Cursor<Document>,
    ) -> PyResult<Cursor> {
        let slot = Arc::new(Slot::new(cursor));
        (db.get().state).adopt(py, &slot)?;
        Ok(Cursor {
            db,
            slot,
            ended: AtomicBool::new(false),
        })
    }
}

#[pymethods]
impl Cursor {
    fn __iter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __next__(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        self.db.get().state.get()?;
        let slot = &self.slot;
        let ended = &self.ended;
        let next = py.detach(|| {
            let mut cursor = slot.lock();
            match cursor.as_mut() {
                None => Err(()),
                Some(open) => {
                    let next = open.next();
                    if next.is_none() {
                        *cursor = None;
                        ended.store(true, Ordering::Relaxed);
                    }
                    Ok(next)
                }
            }
        });
        match next {
            Err(()) if self.ended.load(Ordering::Relaxed) => Ok(None),
            Err(()) => Err(errors::closed("cursor")),
            Ok(None) => Ok(None),
            Ok(Some(Err(e))) => Err(errors::from_db(py, e)),
            Ok(Some(Ok(doc))) => Ok(Some(convert::document_out(py, &doc)?.unbind())),
        }
    }

    /// Lets go of what the cursor keeps. A second close does nothing.
    fn close(&self) {
        self.slot.close();
    }

    #[getter]
    fn closed(&self) -> bool {
        !self.slot.is_open()
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    #[pyo3(signature = (*_args))]
    fn __exit__(&self, _args: &Bound<'_, pyo3::types::PyTuple>) -> bool {
        self.slot.close();
        false
    }
}

impl Drop for Cursor {
    fn drop(&mut self) {
        if self.slot.is_open() {
            Python::attach(|py| warn_unclosed(py, "cursor"));
            self.slot.close();
        }
    }
}

/// What an upsert wrote: the document's id, and whether it was inserted.
#[pyclass(name = "Upserted", module = "trunkdb", frozen, get_all, eq)]
#[derive(PartialEq)]
pub(crate) struct Upserted {
    pub(crate) id: PyDocId,
    pub(crate) inserted: bool,
}

#[pymethods]
impl Upserted {
    fn __repr__(&self) -> String {
        format!(
            "Upserted(id=DocId('{}'), inserted={})",
            self.id.0,
            if self.inserted { "True" } else { "False" }
        )
    }
}
