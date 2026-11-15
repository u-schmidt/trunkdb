//! Changes as they happen (SPEC §197.8): a live query, a live aggregate
//! and a subscription, each a feed read by `next(timeout)`, `for` and
//! `async for`, its events the dataclasses of `trunkdb._live`.
//!
//! Every wait is in slices of 100 ms without the GIL, the feed's lock
//! held only for one slice: between two, Python looks for a signal, and
//! a `close` on another thread gets in.

use crate::convert::{self, PyDocId};
use crate::database::lock;
use crate::errors;
use db::{Change, Commit, Document, Live, LiveRow};
use pyo3::exceptions::{PyRuntimeError, PyStopAsyncIteration, PyValueError};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyModule, PyTuple};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The longest a wait goes without looking for a signal or a close.
const SLICE: Duration = Duration::from_millis(100);

/// What a feed reads from.
pub(crate) enum Source {
    Query(db::LiveQuery<Document>),
    Aggregate(db::LiveAggregate),
    Changes(db::Changes),
}

/// One thing a feed hands out.
enum Item {
    Live(Live<Document>),
    Row(LiveRow),
    Commit(Commit),
}

/// An event, or the error that ended the feed.
type Got = db::Result<Item>;

impl Source {
    /// The next item, waiting at most `wait`; `None` if none came.
    fn recv(&self, wait: Duration) -> Option<Got> {
        if wait.is_zero() {
            return match self {
                Source::Query(q) => q.try_recv().map(|r| r.map(Item::Live)),
                Source::Aggregate(a) => a.try_recv().map(|r| r.map(Item::Row)),
                Source::Changes(c) => c.try_recv().map(|c| Ok(Item::Commit(c))),
            };
        }
        match self {
            Source::Query(q) => q.recv_timeout(wait).map(|r| r.map(Item::Live)),
            Source::Aggregate(a) => a.recv_timeout(wait).map(|r| r.map(Item::Row)),
            Source::Changes(c) => c.recv_timeout(wait).map(|c| Ok(Item::Commit(c))),
        }
    }

    fn has_ended(&self) -> bool {
        match self {
            Source::Query(q) => q.has_ended(),
            Source::Aggregate(a) => a.has_ended(),
            Source::Changes(c) => c.has_ended(),
        }
    }
}

/// What one slice of a wait came to: an item, taken by the slice's
/// `offer`; nothing yet; or the end.
enum Sliced<T> {
    Offered(T),
    Nothing,
    End,
}

/// What a wait of `async for` came to.
enum Came {
    Got(Got),
    End,
}

/// A feed, shared by its class and by the waits of its `async for`.
pub(crate) struct Core {
    /// `None` once closed. Held for one slice at a time.
    source: Mutex<Option<Source>>,
    /// Items read for an `async for` whose wait was cancelled: handed
    /// out first, before anything new.
    kept: Mutex<VecDeque<Got>>,
    /// The source ended, or the feed closed: a `None` from it is the end.
    source_ended: AtomicBool,
    closed: AtomicBool,
}

impl Core {
    fn new(source: Source) -> Arc<Core> {
        Arc::new(Core {
            source: Mutex::new(Some(source)),
            kept: Mutex::new(VecDeque::new()),
            source_ended: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        })
    }

    /// One slice: what was kept, else what comes within `wait`. `offer`
    /// takes an item while the source is still held, so nothing read
    /// after it can be handed out before it.
    fn slice<T>(&self, wait: Duration, offer: impl FnOnce(Got) -> T) -> Sliced<T> {
        let source = lock(&self.source);
        let kept = lock(&self.kept).pop_front();
        if let Some(kept) = kept {
            return Sliced::Offered(offer(kept));
        }
        let Some(open) = source.as_ref() else {
            self.source_ended.store(true, Ordering::Relaxed);
            return Sliced::End;
        };
        match open.recv(wait) {
            Some(got) => Sliced::Offered(offer(got)),
            None if open.has_ended() => {
                self.source_ended.store(true, Ordering::Relaxed);
                Sliced::End
            }
            None => Sliced::Nothing,
        }
    }

    /// `next(timeout)`: an event, `None` at the end or when `timeout`
    /// passed.
    fn next(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<Option<Py<PyAny>>> {
        let deadline = match timeout {
            None => None,
            Some(t) if t.is_finite() && t >= 0.0 => {
                Some(Instant::now() + Duration::from_secs_f64(t.min(1e9)))
            }
            Some(_) => {
                return Err(PyValueError::new_err(
                    "timeout is a number of seconds, 0 or more, or None to wait as long as it takes",
                ));
            }
        };
        loop {
            let wait = match deadline {
                None => SLICE,
                Some(at) => SLICE.min(at.saturating_duration_since(Instant::now())),
            };
            match py.detach(|| self.slice(wait, |got| got)) {
                Sliced::Offered(got) => return event_out(py, got).map(Some),
                Sliced::End => return Ok(None),
                Sliced::Nothing => {
                    if deadline.is_some_and(|at| Instant::now() >= at) {
                        return Ok(None);
                    }
                    py.check_signals()?;
                }
            }
        }
    }

    /// Ended, every event read: what was kept counts.
    fn ended(&self) -> bool {
        self.source_ended.load(Ordering::Relaxed) && lock(&self.kept).is_empty()
    }

    /// `got` put back in front, read first by the next read; let go if
    /// the feed has closed.
    fn keep(&self, got: Got) {
        let mut kept = lock(&self.kept);
        if !self.closed.load(Ordering::Relaxed) {
            kept.push_front(got);
        }
    }

    fn close(&self, py: Python<'_>) {
        py.detach(|| {
            let source = lock(&self.source).take();
            {
                // Closed with what is kept held, so that a wait's event
                // can't be kept after it (`keep`).
                let mut kept = lock(&self.kept);
                kept.clear();
                self.closed.store(true, Ordering::Relaxed);
            }
            self.source_ended.store(true, Ordering::Relaxed);
            drop(source);
        });
    }
}

/// `trunkdb._live`, where the event classes are.
fn events(py: Python<'_>) -> PyResult<&Bound<'_, PyModule>> {
    static MODULE: PyOnceLock<Py<PyModule>> = PyOnceLock::new();
    MODULE
        .get_or_try_init(py, || py.import("trunkdb._live").map(Bound::unbind))
        .map(|m| m.bind(py))
}

/// An item as its event, or the error that ended the feed as its
/// exception.
fn event_out(py: Python<'_>, got: Got) -> PyResult<Py<PyAny>> {
    let item = got.map_err(|e| errors::from_db(py, e))?;
    let classes = events(py)?;
    let make = |class: &str, args: Bound<'_, PyTuple>| -> PyResult<Py<PyAny>> {
        Ok(classes.getattr(class)?.call1(args)?.unbind())
    };
    let doc = |doc: &Document| convert::document_out(py, doc);
    let id = |id: db::DocId| PyDocId(id);
    match item {
        Item::Live(Live::Entered {
            id: i, doc: d, at, ..
        }) => make("Entered", (id(i), doc(&d)?, at).into_pyobject(py)?),
        Item::Live(Live::Left {
            id: i,
            doc: d,
            from,
            ..
        }) => {
            let d = d.as_ref().map(doc).transpose()?;
            make("Left", (id(i), d, from).into_pyobject(py)?)
        }
        Item::Live(Live::Changed {
            id: i,
            doc: d,
            from,
            at,
            ..
        }) => make("Changed", (id(i), doc(&d)?, from, at).into_pyobject(py)?),
        Item::Row(LiveRow::Entered { group, row, .. }) => {
            make("RowEntered", (doc(&group)?, doc(&row)?).into_pyobject(py)?)
        }
        Item::Row(LiveRow::Left { group, row, .. }) => {
            let row = row.as_ref().map(doc).transpose()?;
            make("RowLeft", (doc(&group)?, row).into_pyobject(py)?)
        }
        Item::Row(LiveRow::Changed { group, row, .. }) => {
            make("RowChanged", (doc(&group)?, doc(&row)?).into_pyobject(py)?)
        }
        Item::Commit(commit) => {
            let kind = classes.getattr("ChangeKind")?;
            let change = classes.getattr("Change")?;
            let mut changes = Vec::with_capacity(commit.changes.len());
            for each in &commit.changes {
                let (name, at) = match each {
                    Change::Inserted { .. } => ("inserted", each.id()),
                    Change::Updated { .. } => ("updated", each.id()),
                    Change::Deleted { .. } => ("deleted", each.id()),
                    Change::Dropped { .. } => ("dropped", None),
                    _ => return Err(unknown()),
                };
                changes.push(change.call1((
                    kind.call1((name,))?,
                    each.collection(),
                    at.map(id),
                ))?);
            }
            make("Commit", (PyTuple::new(py, changes)?,).into_pyobject(py)?)
        }
        // Kinds the library gains after these bindings.
        _ => Err(unknown()),
    }
}

fn unknown() -> PyErr {
    PyValueError::new_err("a kind of event this build of the module doesn't know")
}

/// One `__anext__`'s wait, served on the `async for`'s thread: it reads
/// one event, never one ahead, so a loop left by `break` leaves nothing
/// read behind. A wait cancelled once its event came puts it back in
/// front of the feed.
#[pyclass(name = "_Wait", module = "trunkdb", frozen)]
pub(crate) struct Wait {
    core: Arc<Core>,
    state: Mutex<WaitState>,
}

#[derive(Default)]
struct WaitState {
    abandoned: bool,
    came: Option<Came>,
}

#[pymethods]
impl Wait {
    /// On the reader's thread: waits for the event, or the end, or for
    /// the wait to be abandoned.
    fn serve(&self, py: Python<'_>) {
        // What came is handed over while the feed is held, so that no
        // read after it overtakes it.
        let offer = |got: Got| {
            let mut state = lock(&self.state);
            if state.abandoned {
                self.core.keep(got);
            } else {
                state.came = Some(Came::Got(got));
            }
        };
        loop {
            if lock(&self.state).abandoned {
                return;
            }
            match py.detach(|| self.core.slice(SLICE, offer)) {
                Sliced::Nothing => continue,
                Sliced::Offered(()) => return,
                Sliced::End => {
                    let mut state = lock(&self.state);
                    if !state.abandoned {
                        state.came = Some(Came::End);
                    }
                    return;
                }
            }
        }
    }

    /// The loop no longer waits: what came is kept for the next read.
    fn abandon(&self) {
        let mut state = lock(&self.state);
        state.abandoned = true;
        if let Some(Came::Got(got)) = state.came.take() {
            self.core.keep(got);
        }
    }

    /// What came: the event, the feed's error, or `StopAsyncIteration`.
    fn take(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let came = lock(&self.state).came.take();
        match came {
            Some(Came::Got(got)) => event_out(py, got),
            Some(Came::End) => Err(PyStopAsyncIteration::new_err(())),
            None => Err(PyRuntimeError::new_err("a wait taken before it was served")),
        }
    }
}

/// The three feeds' classes: the same methods, over a `Core`.
macro_rules! feed {
    ($rust:ident, $name:literal, $doc:literal) => {
        #[doc = $doc]
        #[pyclass(name = $name, module = "trunkdb", frozen)]
        pub(crate) struct $rust {
            core: Arc<Core>,
            /// Its collection; `None` for a subscription.
            collection: Option<String>,
        }

        impl $rust {
            pub(crate) fn new(source: Source, collection: Option<String>) -> $rust {
                $rust {
                    core: Core::new(source),
                    collection,
                }
            }
        }

        #[pymethods]
        impl $rust {
            /// The next event, waiting at most `timeout` seconds, or as
            /// long as it takes; `None` if none came, or at the end:
            /// `ended` says which.
            #[pyo3(signature = (timeout = None))]
            fn next(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<Option<Py<PyAny>>> {
                self.core.next(py, timeout)
            }

            /// Whether the feed has ended, every event read: closed, its
            /// database closed, or ended by an error.
            #[getter]
            fn ended(&self) -> bool {
                self.core.ended()
            }

            #[getter]
            fn closed(&self) -> bool {
                self.core.closed.load(Ordering::Relaxed)
            }

            /// Ends the feed; what it held and wasn't read is let go.
            fn close(&self, py: Python<'_>) {
                self.core.close(py);
            }

            fn __iter__(slf: Py<Self>) -> Py<Self> {
                slf
            }

            fn __next__(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
                self.core.next(py, None)
            }

            fn __aiter__<'py>(slf: &Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
                events(slf.py())?.getattr("_AsyncReader")?.call1((slf,))
            }

            /// A wait for `async for` (`trunkdb._live`).
            fn _wait(&self) -> Wait {
                Wait {
                    core: self.core.clone(),
                    state: Mutex::new(WaitState::default()),
                }
            }

            fn __enter__(slf: Py<Self>) -> Py<Self> {
                slf
            }

            fn __exit__(
                &self,
                py: Python<'_>,
                _kind: Option<&Bound<'_, PyAny>>,
                _value: Option<&Bound<'_, PyAny>>,
                _traceback: Option<&Bound<'_, PyAny>>,
            ) -> bool {
                self.core.close(py);
                false
            }

            fn __repr__(&self, py: Python<'_>) -> String {
                let closed = if self.closed() { " closed" } else { "" };
                match &self.collection {
                    Some(name) => format!(
                        "<trunkdb.{}{closed} {}>",
                        $name,
                        crate::database::quoted(py, name)
                    ),
                    None => format!("<trunkdb.{}{closed}>", $name),
                }
            }
        }
    };
}

feed!(
    LiveQuery,
    "LiveQuery",
    "A live query, from `Collection.watch`: `Entered`, `Left` and `Changed`."
);
feed!(
    LiveAggregate,
    "LiveAggregate",
    "A live aggregate, from `Collection.watch_aggregate`: `RowEntered`, `RowLeft` and `RowChanged`."
);
feed!(
    Changes,
    "Changes",
    "A subscription, from `Database.subscribe`: a `Commit` for each batch committed."
);
