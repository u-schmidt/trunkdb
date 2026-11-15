//! Documents both ways (SPEC §197.4): a `Document` as Python's own
//! types, and back, every value checked and a mistake named by its path.

use db::{Date, DateTime, DocId, Document, Map, Time};
use pyo3::IntoPyObjectExt;
use pyo3::exceptions::{PyOverflowError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{
    PyBool, PyByteArray, PyBytes, PyDate, PyDateAccess, PyDateTime, PyDelta, PyDeltaAccess, PyDict,
    PyFloat, PyInt, PyList, PyMemoryView, PyString, PyTime, PyTimeAccess, PyTuple, PyTzInfo,
    PyTzInfoAccess,
};

/// A document's id: made from its text, `str()` giving it back; equal,
/// hashed and ordered as trunkdb orders ids, by when they were made.
#[pyclass(name = "DocId", module = "trunkdb", frozen, eq, ord, hash)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct PyDocId(pub(crate) DocId);

#[pymethods]
impl PyDocId {
    /// The id written as `text`: a UUID, with or without its hyphens.
    #[new]
    fn new(text: &str) -> PyResult<Self> {
        text.parse()
            .map(PyDocId)
            .map_err(|e: db::ParseIdError| PyValueError::new_err(e.to_string()))
    }

    fn __str__(&self) -> String {
        self.0.to_string()
    }

    fn __repr__(&self) -> String {
        format!("DocId('{}')", self.0)
    }

    /// Pickled as its text.
    fn __reduce__<'py>(slf: &Bound<'py, Self>) -> PyResult<(Bound<'py, PyAny>, (String,))> {
        Ok((slf.get_type().into_any(), (slf.get().0.to_string(),)))
    }
}

/// Which way a value didn't convert, and where: the path is built as the
/// error goes back up, so a value that converts costs nothing for it.
pub(crate) struct Bad {
    class: fn(String) -> PyErr,
    message: String,
    /// Innermost first.
    path: Vec<Step>,
}

enum Step {
    Key(String),
    Index(usize),
}

impl Bad {
    fn new(class: fn(String) -> PyErr, message: impl Into<String>) -> Bad {
        Bad {
            class,
            message: message.into(),
            path: Vec::new(),
        }
    }

    fn key(mut self, key: &str) -> Bad {
        self.path.push(Step::Key(key.to_string()));
        self
    }

    fn index(mut self, at: usize) -> Bad {
        self.path.push(Step::Index(at));
        self
    }

    /// The exception, its message the path from `root`, as
    /// `[3].due` or `:today`, then what was wrong.
    pub(crate) fn into_err(self, root: &str) -> PyErr {
        let mut path = root.to_string();
        for step in self.path.iter().rev() {
            match step {
                Step::Key(key) if path.is_empty() => path.push_str(key),
                Step::Key(key) => {
                    path.push('.');
                    path.push_str(key);
                }
                Step::Index(at) => path.push_str(&format!("[{at}]")),
            }
        }
        if path.is_empty() {
            path.push_str("the document");
        }
        (self.class)(format!("{path}: {}", self.message))
    }
}

fn type_error(message: String) -> PyErr {
    PyTypeError::new_err(message)
}

fn value_error(message: String) -> PyErr {
    PyValueError::new_err(message)
}

fn overflow_error(message: String) -> PyErr {
    PyOverflowError::new_err(message)
}

/// A Python error met while reading a value, as a `Bad` of its class.
fn from_py(py: Python<'_>, error: PyErr) -> Bad {
    let class = if error.is_instance_of::<PyOverflowError>(py) {
        overflow_error
    } else if error.is_instance_of::<PyTypeError>(py) {
        type_error
    } else {
        value_error
    };
    Bad::new(class, error.value(py).to_string())
}

// ---- Into trunkdb ----

/// A dict as a document's object, its keys `str` only.
pub(crate) fn dict_to_map(dict: &Bound<'_, PyDict>) -> Result<Map, Bad> {
    let mut map = Map::with_capacity(dict.len());
    for (key, value) in dict.iter() {
        let Ok(key) = key.cast::<PyString>() else {
            let shown = key.repr().map(|r| r.to_string()).unwrap_or_default();
            return Err(Bad::new(
                type_error,
                format!(
                    "a key that is a {}, {shown}: a document's keys are str",
                    type_name(&key)
                ),
            ));
        };
        let key = key.to_str().map_err(|e| from_py(dict.py(), e))?;
        let value = to_document(&value).map_err(|bad| bad.key(key))?;
        map.insert(key.to_string(), value);
    }
    Ok(map)
}

/// A Python value as a document's value.
pub(crate) fn to_document(value: &Bound<'_, PyAny>) -> Result<Document, Bad> {
    let py = value.py();
    if value.is_none() {
        return Ok(Document::Null);
    }
    // Before `int`: Python's `bool` is one.
    if let Ok(flag) = value.cast::<PyBool>() {
        return Ok(Document::Bool(flag.is_true()));
    }
    if value.is_instance_of::<PyInt>() {
        return value.extract::<i64>().map(Document::Int).map_err(|_| {
            Bad::new(
                overflow_error,
                format!(
                    "{} is outside what trunkdb's whole numbers hold, -2**63 to 2**63 - 1",
                    value.str().map(|s| s.to_string()).unwrap_or_default()
                ),
            )
        });
    }
    if let Ok(float) = value.cast::<PyFloat>() {
        return Ok(Document::Float(float.value()));
    }
    if let Ok(text) = value.cast::<PyString>() {
        return text
            .to_str()
            .map(|text| Document::String(text.to_string()))
            .map_err(|e| from_py(py, e));
    }
    if let Ok(bytes) = value.cast::<PyBytes>() {
        return Ok(Document::Binary(bytes.as_bytes().to_vec()));
    }
    if let Ok(bytes) = value.cast::<PyByteArray>() {
        return Ok(Document::Binary(bytes.to_vec()));
    }
    if value.is_instance_of::<PyMemoryView>() {
        let bytes = value
            .call_method0("tobytes")
            .and_then(|b| b.extract::<Vec<u8>>())
            .map_err(|e| from_py(py, e))?;
        return Ok(Document::Binary(bytes));
    }
    if let Ok(dict) = value.cast::<PyDict>() {
        return dict_to_map(dict).map(Document::Object);
    }
    if let Ok(list) = value.cast::<PyList>() {
        return (list.iter().enumerate())
            .map(|(at, item)| to_document(&item).map_err(|bad| bad.index(at)))
            .collect::<Result<_, _>>()
            .map(Document::Array);
    }
    if let Ok(tuple) = value.cast::<PyTuple>() {
        return (tuple.iter().enumerate())
            .map(|(at, item)| to_document(&item).map_err(|bad| bad.index(at)))
            .collect::<Result<_, _>>()
            .map(Document::Array);
    }
    if let Ok(id) = value.cast::<PyDocId>() {
        return Ok(Document::Id(id.get().0));
    }
    // Before `date`: a `datetime` is one.
    if let Ok(when) = value.cast::<PyDateTime>() {
        return date_time_in(when).map(Document::DateTime);
    }
    if let Ok(day) = value.cast::<PyDate>() {
        return Date::from_ymd(
            i64::from(day.get_year()),
            u32::from(day.get_month()),
            u32::from(day.get_day()),
        )
        .map(Document::Date)
        .ok_or_else(|| Bad::new(value_error, "a date trunkdb can't hold"));
    }
    if let Ok(time) = value.cast::<PyTime>() {
        if time.get_tzinfo().is_some() {
            return Err(Bad::new(
                value_error,
                "a time with a tzinfo: trunkdb's times of day have no zone",
            ));
        }
        return time_of(
            time.get_hour(),
            time.get_minute(),
            time.get_second(),
            time.get_microsecond(),
        )
        .map(Document::Time);
    }
    Err(Bad::new(
        type_error,
        format!(
            "a {} is not a value trunkdb stores: None, bool, int, float, str, bytes, list, \
             dict, DocId, datetime, date or time",
            type_name(value)
        ),
    ))
}

fn type_name(value: &Bound<'_, PyAny>) -> String {
    value
        .get_type()
        .name()
        .map(|n| n.to_string())
        .unwrap_or_else(|_| "value".to_string())
}

fn time_of(hour: u8, minute: u8, second: u8, micro: u32) -> Result<Time, Bad> {
    Time::from_hms_nano(
        u32::from(hour),
        u32::from(minute),
        u32::from(second),
        micro * 1000,
    )
    .ok_or_else(|| Bad::new(value_error, "a time trunkdb can't hold"))
}

/// A `datetime` with a zone as trunkdb's, in UTC; one without refused.
fn date_time_in(when: &Bound<'_, PyDateTime>) -> Result<DateTime, Bad> {
    let py = when.py();
    let offset = match when.get_tzinfo() {
        None => None,
        Some(_) => {
            let offset = when.call_method0("utcoffset").map_err(|e| from_py(py, e))?;
            match offset.cast::<PyDelta>() {
                Ok(delta) => Some(
                    i128::from(delta.get_days()) * 86_400_000_000
                        + i128::from(delta.get_seconds()) * 1_000_000
                        + i128::from(delta.get_microseconds()),
                ),
                Err(_) => None,
            }
        }
    };
    let Some(offset_micros) = offset else {
        return Err(Bad::new(
            value_error,
            "a datetime without a tzinfo: say which zone it is in, as \
             datetime.now(timezone.utc) or .replace(tzinfo=timezone.utc)",
        ));
    };
    let day = Date::from_ymd(
        i64::from(when.get_year()),
        u32::from(when.get_month()),
        u32::from(when.get_day()),
    );
    let time = time_of(
        when.get_hour(),
        when.get_minute(),
        when.get_second(),
        when.get_microsecond(),
    )?;
    let local = day
        .and_then(|day| day.at(time))
        .ok_or_else(|| Bad::new(value_error, "a datetime trunkdb can't hold"))?;
    // The wall time less its offset from UTC, in nanoseconds.
    let nanos = i128::from(local.unix_seconds()) * 1_000_000_000 + i128::from(local.subsec_nanos())
        - offset_micros * 1000;
    let secs = i64::try_from(nanos.div_euclid(1_000_000_000));
    let subsec = u32::try_from(nanos.rem_euclid(1_000_000_000));
    match (secs, subsec) {
        (Ok(secs), Ok(subsec)) => DateTime::from_unix(secs, subsec),
        _ => None,
    }
    .ok_or_else(|| Bad::new(value_error, "a datetime trunkdb can't hold"))
}

// ---- Out of trunkdb ----

/// A document's value as Python's.
pub(crate) fn to_py<'py>(py: Python<'py>, value: &Document) -> Result<Bound<'py, PyAny>, Bad> {
    let made = match value {
        Document::Null => Ok(py.None().into_bound(py)),
        Document::Bool(flag) => flag.into_bound_py_any(py),
        Document::Int(n) => n.into_bound_py_any(py),
        Document::Float(x) => x.into_bound_py_any(py),
        Document::String(text) => text.into_bound_py_any(py),
        Document::Binary(bytes) => Ok(PyBytes::new(py, bytes).into_any()),
        Document::Array(items) => {
            let list = PyList::empty(py);
            for (at, item) in items.iter().enumerate() {
                let item = to_py(py, item).map_err(|bad| bad.index(at))?;
                list.append(item).map_err(|e| from_py(py, e))?;
            }
            Ok(list.into_any())
        }
        Document::Object(map) => return map_to_dict(py, map).map(Bound::into_any),
        Document::Id(id) => PyDocId(*id).into_bound_py_any(py),
        Document::DateTime(when) => return date_time_out(py, when),
        Document::Date(day) => {
            let (year, month, day) = ymd(day)?;
            PyDate::new(py, year, month, day).map(Bound::into_any)
        }
        Document::Time(time) => PyTime::new(
            py,
            time.hour() as u8,
            time.minute() as u8,
            time.second() as u8,
            time.nanosecond() / 1000,
            None,
        )
        .map(Bound::into_any),
        // A kind of value added to trunkdb after these bindings.
        other => {
            return Err(Bad::new(
                value_error,
                format!("{other}: a kind of value these bindings don't know"),
            ));
        }
    };
    made.map_err(|e| from_py(py, e))
}

/// A document's object as a dict, its fields in their order.
pub(crate) fn map_to_dict<'py>(py: Python<'py>, map: &Map) -> Result<Bound<'py, PyDict>, Bad> {
    let dict = PyDict::new(py);
    for (key, value) in map {
        let value = to_py(py, value).map_err(|bad| bad.key(key))?;
        dict.set_item(key, value).map_err(|e| from_py(py, e))?;
    }
    Ok(dict)
}

/// A date's year, month and day, where Python's `date` holds it: years 1
/// to 9999.
fn ymd(day: &Date) -> Result<(i32, u8, u8), Bad> {
    match i32::try_from(day.year()) {
        Ok(year @ 1..=9999) => Ok((year, day.month() as u8, day.day() as u8)),
        _ => Err(Bad::new(
            value_error,
            format!("{day} is outside the years 1 to 9999, which Python's dates hold"),
        )),
    }
}

/// A point in time as an aware `datetime` in UTC, its nanoseconds cut to
/// microseconds.
fn date_time_out<'py>(py: Python<'py>, when: &DateTime) -> Result<Bound<'py, PyAny>, Bad> {
    let (year, month, day) = ymd(&when.date()).map_err(|_| {
        Bad::new(
            value_error,
            format!("{when} is outside the years 1 to 9999, which Python's datetimes hold"),
        )
    })?;
    let time = when.time();
    let utc = PyTzInfo::utc(py).map_err(|e| from_py(py, e))?;
    PyDateTime::new(
        py,
        year,
        month,
        day,
        time.hour() as u8,
        time.minute() as u8,
        time.second() as u8,
        time.nanosecond() / 1000,
        Some(&utc),
    )
    .map(Bound::into_any)
    .map_err(|e| from_py(py, e))
}

/// A document handed to Python: a dict for an object, else the value.
pub(crate) fn document_out<'py>(py: Python<'py>, doc: &Document) -> PyResult<Bound<'py, PyAny>> {
    to_py(py, doc).map_err(|bad| bad.into_err(""))
}

/// Documents handed to Python, each named by its place in the list.
pub(crate) fn documents_out<'py>(
    py: Python<'py>,
    docs: &[Document],
) -> PyResult<Bound<'py, PyList>> {
    let list = PyList::empty(py);
    for (at, doc) in docs.iter().enumerate() {
        let item = to_py(py, doc).map_err(|bad| bad.index(at).into_err(""))?;
        list.append(item)?;
    }
    Ok(list)
}

/// A dict given as a document: an object, its mistakes named from the
/// document.
pub(crate) fn document_in(value: &Bound<'_, PyAny>, root: &str) -> PyResult<Document> {
    match value.cast::<PyDict>() {
        Ok(dict) => dict_to_map(dict)
            .map(Document::Object)
            .map_err(|bad| bad.into_err(root)),
        Err(_) => Err(PyTypeError::new_err(format!(
            "{}a document is a dict, not a {}",
            if root.is_empty() {
                String::new()
            } else {
                format!("{root}: ")
            },
            type_name(value)
        ))),
    }
}

/// An id given as a `DocId`, or as its text.
pub(crate) fn id_in(value: &Bound<'_, PyAny>) -> PyResult<DocId> {
    if let Ok(id) = value.cast::<PyDocId>() {
        return Ok(id.get().0);
    }
    if let Ok(text) = value.cast::<PyString>() {
        return PyDocId::new(text.to_str()?).map(|id| id.0);
    }
    Err(PyTypeError::new_err(format!(
        "an id is a DocId or its text, not a {}",
        type_name(value)
    )))
}

/// A text's parameters' values, a dict of names to values, each value
/// converted as a document's; `None` for none.
pub(crate) fn params_in(value: Option<&Bound<'_, PyAny>>) -> PyResult<db::query::Params> {
    let mut params = db::query::Params::new();
    let Some(value) = value.filter(|v| !v.is_none()) else {
        return Ok(params);
    };
    let Ok(dict) = value.cast::<PyDict>() else {
        return Err(PyTypeError::new_err(format!(
            "params is a dict of names to values, not a {}",
            type_name(value)
        )));
    };
    for (name, value) in dict.iter() {
        let Ok(name) = name.cast::<PyString>() else {
            return Err(PyTypeError::new_err(format!(
                "params: a name that is a {}: a parameter's name is a str",
                type_name(&name)
            )));
        };
        let name = name.to_str()?;
        let value = to_document(&value).map_err(|bad| bad.into_err(&format!(":{name}")))?;
        params = params.set(name, value);
    }
    Ok(params)
}
