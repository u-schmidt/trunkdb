//! trunkdb's errors as Python's exceptions (SPEC §197.5): a class for
//! each kind a program catches, its details as attributes.

use pyo3::exceptions::{PyBlockingIOError, PyException, PyOSError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::{IntoPyObjectExt, PyTypeInfo, create_exception};

create_exception!(trunkdb, Error, PyException, "Every error of trunkdb's own.");
create_exception!(
    trunkdb,
    ParseError,
    Error,
    "A text that doesn't read: a filter, an aggregate, an update's operators, a batch's line."
);
create_exception!(
    trunkdb,
    DuplicateIdError,
    Error,
    "The collection already has a document with this id."
);
create_exception!(
    trunkdb,
    DuplicateValueError,
    Error,
    "A unique index already has the document's value."
);
create_exception!(trunkdb, NotFoundError, Error, "No document has the id.");
create_exception!(
    trunkdb,
    MultipleMatchesError,
    Error,
    "An upsert's filter matched more than one document."
);
create_exception!(
    trunkdb,
    UpdateError,
    Error,
    "An update's operator that can't be made to the document."
);
create_exception!(
    trunkdb,
    PreconditionFailedError,
    Error,
    "A batch's `require` or `require_none` failed."
);
create_exception!(
    trunkdb,
    NotLiveError,
    Error,
    "A filter a live query can't keep."
);
create_exception!(
    trunkdb,
    SnapshotTooOldError,
    Error,
    "A snapshot kept past the memory snapshots may take; take a new one."
);
create_exception!(
    trunkdb,
    ReadOnlyError,
    Error,
    "A write to a database opened read-only."
);
create_exception!(
    trunkdb,
    FormatError,
    Error,
    "A file that isn't trunkdb's, or of a format this build can't read."
);

/// The classes, for the module.
pub(crate) fn add(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add("Error", py.get_type::<Error>())?;
    m.add("ParseError", py.get_type::<ParseError>())?;
    m.add("DuplicateIdError", py.get_type::<DuplicateIdError>())?;
    m.add("DuplicateValueError", py.get_type::<DuplicateValueError>())?;
    m.add("NotFoundError", py.get_type::<NotFoundError>())?;
    m.add(
        "MultipleMatchesError",
        py.get_type::<MultipleMatchesError>(),
    )?;
    m.add("UpdateError", py.get_type::<UpdateError>())?;
    m.add(
        "PreconditionFailedError",
        py.get_type::<PreconditionFailedError>(),
    )?;
    m.add("NotLiveError", py.get_type::<NotLiveError>())?;
    m.add("SnapshotTooOldError", py.get_type::<SnapshotTooOldError>())?;
    m.add("ReadOnlyError", py.get_type::<ReadOnlyError>())?;
    m.add("FormatError", py.get_type::<FormatError>())?;
    Ok(())
}

/// An attribute's value: text, a number, true or false, or `None`.
pub(crate) enum Detail {
    None,
    Text(String),
    Count(usize),
    Flag(bool),
    Id(db::DocId),
}

impl From<&str> for Detail {
    fn from(text: &str) -> Detail {
        Detail::Text(text.to_string())
    }
}

impl From<String> for Detail {
    fn from(text: String) -> Detail {
        Detail::Text(text)
    }
}

impl From<usize> for Detail {
    fn from(n: usize) -> Detail {
        Detail::Count(n)
    }
}

impl<T: Into<Detail>> From<Option<T>> for Detail {
    fn from(value: Option<T>) -> Detail {
        value.map_or(Detail::None, Into::into)
    }
}

/// An exception of class `T` with `message`, its `kind` and `details`
/// set as attributes.
pub(crate) fn raise<T: PyTypeInfo>(
    py: Python<'_>,
    kind: &str,
    message: impl Into<String>,
    details: Vec<(&str, Detail)>,
) -> PyErr {
    let error = PyErr::from_type(T::type_object(py), message.into());
    let value = error.value(py);
    let set = |name: &str, detail: Detail| -> PyResult<()> {
        let value_of = match detail {
            Detail::None => py.None(),
            Detail::Text(text) => text.into_py_any(py)?,
            Detail::Count(n) => n.into_py_any(py)?,
            Detail::Flag(flag) => flag.into_py_any(py)?,
            Detail::Id(id) => crate::convert::PyDocId(id).into_py_any(py)?,
        };
        value.setattr(name, value_of)
    };
    // Setting an attribute on a new exception doesn't fail; if it did,
    // the error would still say what went wrong.
    let _ = set("kind", Detail::from(kind));
    for (name, detail) in details {
        let _ = set(name, detail);
    }
    error
}

/// A text that doesn't read: `what` it was, its `text`, where in it, and
/// on which line of a batch.
pub(crate) struct Parse {
    pub(crate) what: &'static str,
    pub(crate) message: String,
    pub(crate) text: Option<String>,
    pub(crate) offset: Option<usize>,
    pub(crate) line: Option<usize>,
    pub(crate) parameter: Option<String>,
}

impl Parse {
    pub(crate) fn new(what: &'static str, message: impl std::fmt::Display) -> Parse {
        Parse {
            what,
            message: message.to_string(),
            text: None,
            offset: None,
            line: None,
            parameter: None,
        }
    }

    pub(crate) fn into_err(self, py: Python<'_>) -> PyErr {
        raise::<ParseError>(
            py,
            "parse",
            format!("{}: {}", self.what, self.message),
            vec![
                ("what", self.what.into()),
                ("text", self.text.into()),
                ("offset", self.offset.into()),
                ("line", self.line.into()),
                ("parameter", self.parameter.into()),
            ],
        )
    }
}

/// An I/O error as Python's own: `OSError` with its `errno`, the class
/// Python picks for it (`FileNotFoundError`, `BlockingIOError`…), and
/// `path` as its file name where known.
pub(crate) fn io(py: Python<'_>, error: std::io::Error, path: Option<&str>) -> PyErr {
    if error.kind() == std::io::ErrorKind::InvalidInput {
        // What the library refuses as asked: the call's mistake.
        return PyValueError::new_err(error.to_string());
    }
    match (error.raw_os_error(), path) {
        // `OSError(errno, message, path)` makes the subclass of `errno`.
        (Some(errno), Some(path)) => {
            let message = std::io::Error::from_raw_os_error(errno).to_string();
            match PyOSError::type_object(py).call1((errno, message, path)) {
                Ok(made) => PyErr::from_value(made),
                Err(e) => e,
            }
        }
        // The file in use: `EAGAIN`, as Python's own lock calls say it.
        (None, _) if error.kind() == std::io::ErrorKind::WouldBlock => {
            let errno = (py.import("errno").and_then(|m| m.getattr("EAGAIN")))
                .and_then(|n| n.extract::<i32>());
            let args = match errno {
                Ok(errno) => (Some(errno), error.to_string(), path),
                Err(_) => (None, error.to_string(), path),
            };
            match PyBlockingIOError::type_object(py).call1(args) {
                Ok(made) => PyErr::from_value(made),
                Err(e) => e,
            }
        }
        _ => PyErr::from(error),
    }
}

/// `error` as its exception.
pub(crate) fn from_db(py: Python<'_>, error: db::Error) -> PyErr {
    use db::Error as E;
    let message = error.to_string();
    match error {
        E::Io(e) => io(py, e, None),
        E::Format(_) => raise::<FormatError>(py, "format", message, vec![]),
        E::ReadOnly => raise::<ReadOnlyError>(py, "read_only", message, vec![]),
        E::SnapshotTooOld => raise::<SnapshotTooOldError>(py, "snapshot_too_old", message, vec![]),
        E::DuplicateId { collection, id, .. } => raise::<DuplicateIdError>(
            py,
            "duplicate_id",
            message,
            vec![("collection", collection.into()), ("id", Detail::Id(id))],
        ),
        E::DuplicateValue {
            collection,
            field,
            id,
            existing,
            ..
        } => raise::<DuplicateValueError>(
            py,
            "duplicate_value",
            message,
            vec![
                ("collection", collection.into()),
                ("field", field.into()),
                ("id", Detail::Id(id)),
                ("existing", Detail::Id(existing)),
            ],
        ),
        E::NotFound { collection, id, .. } => raise::<NotFoundError>(
            py,
            "not_found",
            message,
            vec![("collection", collection.into()), ("id", Detail::Id(id))],
        ),
        E::MultipleMatches {
            collection, count, ..
        } => raise::<MultipleMatchesError>(
            py,
            "multiple_matches",
            message,
            vec![("collection", collection.into()), ("count", count.into())],
        ),
        E::Update {
            collection,
            id,
            message: why,
            ..
        } => raise::<UpdateError>(
            py,
            "update",
            message,
            vec![
                ("collection", collection.into()),
                ("id", Detail::Id(id)),
                ("message", why.into()),
            ],
        ),
        E::PreconditionFailed {
            collection,
            op,
            matched,
            ..
        } => raise::<PreconditionFailedError>(
            py,
            "precondition_failed",
            message,
            vec![
                ("collection", collection.into()),
                ("op", op.into()),
                ("matched", Detail::Flag(matched)),
                ("line", Detail::None),
            ],
        ),
        E::NotLive { reason, .. } => {
            raise::<NotLiveError>(py, "not_live", message, vec![("reason", reason.into())])
        }
        // A document that isn't an object can't come from a dict; a
        // serde error can't come from a `Document`.
        E::NotAnObject { .. } | E::Document(_) => PyTypeError::new_err(message),
        E::Poisoned => raise::<Error>(py, "poisoned", message, vec![]),
        E::SnapshotOpen => raise::<Error>(py, "snapshot_open", message, vec![]),
        E::Hint {
            collection, index, ..
        } => raise::<Error>(
            py,
            "hint",
            message,
            vec![("collection", collection.into()), ("index", index.into())],
        ),
        E::Import { line, .. } => {
            raise::<Error>(py, "import", message, vec![("line", line.into())])
        }
        _ => raise::<Error>(py, "other", message, vec![]),
    }
}

/// `ValueError("closed database")`, and the like for a snapshot or a
/// cursor: as a closed file's.
pub(crate) fn closed(what: &str) -> PyErr {
    PyValueError::new_err(format!("closed {what}"))
}
