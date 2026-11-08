//! Errors as codes, and each thread's last one as text and as JSON
//! (SPEC §177.4, §179).

use crate::text::c_string;
use crate::*;
use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::ffi::CString;
use std::panic::{AssertUnwindSafe, catch_unwind};

/// Why a call failed: its code, its message, and what else is known.
#[derive(Debug)]
pub(crate) struct Fail {
    pub(crate) code: i32,
    message: String,
    detail: Map<String, Value>,
}

impl Fail {
    pub(crate) fn new(code: i32, message: impl Into<String>) -> Fail {
        Fail {
            code,
            message: message.into(),
            detail: Map::new(),
        }
    }

    /// A pointer or a text that isn't what the call takes.
    pub(crate) fn argument(message: impl Into<String>) -> Fail {
        Fail::new(TRUNKDB_INVALID_ARGUMENT, message)
    }

    /// A text that doesn't read: `what` it was, and why.
    pub(crate) fn parse(what: &str, message: impl std::fmt::Display) -> Fail {
        Fail::new(TRUNKDB_PARSE, format!("{what}: {message}")).with("what", what)
    }

    /// `key` added to the details.
    pub(crate) fn with(mut self, key: &str, value: impl Into<Value>) -> Fail {
        self.detail.insert(key.to_string(), value.into());
        self
    }
}

/// The name of each code, as the details give it.
fn kind(code: i32) -> &'static str {
    match code {
        TRUNKDB_INVALID_ARGUMENT => "invalid_argument",
        TRUNKDB_PARSE => "parse",
        TRUNKDB_IO => "io",
        TRUNKDB_FORMAT => "format",
        TRUNKDB_READ_ONLY => "read_only",
        TRUNKDB_POISONED => "poisoned",
        TRUNKDB_DUPLICATE_ID => "duplicate_id",
        TRUNKDB_DUPLICATE_VALUE => "duplicate_value",
        TRUNKDB_NOT_FOUND => "not_found",
        TRUNKDB_MULTIPLE_MATCHES => "multiple_matches",
        TRUNKDB_UPDATE => "update",
        TRUNKDB_PRECONDITION_FAILED => "precondition_failed",
        TRUNKDB_NOT_AN_OBJECT => "not_an_object",
        TRUNKDB_HINT => "hint",
        TRUNKDB_IMPORT => "import",
        TRUNKDB_SNAPSHOT_OPEN => "snapshot_open",
        TRUNKDB_SNAPSHOT_TOO_OLD => "snapshot_too_old",
        TRUNKDB_PANIC => "panic",
        _ => "other",
    }
}

/// Each `Error` as its code, every field it has in the details.
impl From<db::Error> for Fail {
    fn from(error: db::Error) -> Fail {
        use db::{Error, FormatError};
        let message = error.to_string();
        let fail = |code| Fail::new(code, message.clone());
        match &error {
            // What the library refuses as asked, an index of other
            // options, a precondition with a limit, is the call's
            // mistake, not the file system's.
            Error::Io(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
                fail(TRUNKDB_INVALID_ARGUMENT).with("io", "InvalidInput")
            }
            Error::Io(e) => fail(TRUNKDB_IO).with("io", format!("{:?}", e.kind())),
            Error::Format(e) => {
                let fail = fail(TRUNKDB_FORMAT);
                match e {
                    FormatError::Newer { found, current, .. } => {
                        fail.with("found", *found).with("current", *current)
                    }
                    FormatError::Older { found, current, .. } => {
                        fail.with("found", *found).with("current", *current)
                    }
                    _ => fail,
                }
            }
            Error::ReadOnly => fail(TRUNKDB_READ_ONLY),
            Error::Poisoned => fail(TRUNKDB_POISONED),
            Error::SnapshotOpen => fail(TRUNKDB_SNAPSHOT_OPEN),
            Error::SnapshotTooOld => fail(TRUNKDB_SNAPSHOT_TOO_OLD),
            Error::DuplicateId { collection, id, .. } => fail(TRUNKDB_DUPLICATE_ID)
                .with("collection", collection.as_str())
                .with("id", id.to_string()),
            Error::DuplicateValue {
                collection,
                field,
                id,
                existing,
                ..
            } => fail(TRUNKDB_DUPLICATE_VALUE)
                .with("collection", collection.as_str())
                .with("field", field.as_str())
                .with("id", id.to_string())
                .with("existing", existing.to_string()),
            Error::NotFound { collection, id, .. } => fail(TRUNKDB_NOT_FOUND)
                .with("collection", collection.as_str())
                .with("id", id.to_string()),
            Error::MultipleMatches {
                collection, count, ..
            } => fail(TRUNKDB_MULTIPLE_MATCHES)
                .with("collection", collection.as_str())
                .with("count", *count),
            Error::Update { collection, id, .. } => fail(TRUNKDB_UPDATE)
                .with("collection", collection.as_str())
                .with("id", id.to_string()),
            Error::PreconditionFailed {
                collection,
                op,
                matched,
                ..
            } => fail(TRUNKDB_PRECONDITION_FAILED)
                .with("collection", collection.as_str())
                .with("op", *op)
                .with("matched", *matched),
            Error::NotAnObject { collection, .. } => {
                fail(TRUNKDB_NOT_AN_OBJECT).with("collection", collection.as_str())
            }
            Error::Hint {
                collection, index, ..
            } => fail(TRUNKDB_HINT)
                .with("collection", collection.as_str())
                .with("index", index.as_str()),
            Error::Import { line, .. } => fail(TRUNKDB_IMPORT).with("line", *line),
            // A closure's panic, a live query: none of them made through
            // these functions; and kinds added later.
            _ => fail(TRUNKDB_OTHER),
        }
    }
}

thread_local! {
    /// This thread's last error: its message, and its details as JSON.
    static LAST: RefCell<(CString, CString)> = RefCell::new((CString::default(), c_string("{}".to_string())));
}

/// `fail` kept as this thread's last error; its code.
fn record(fail: Fail) -> i32 {
    let mut detail = Map::new();
    detail.insert("code".to_string(), json!(fail.code));
    detail.insert("kind".to_string(), json!(kind(fail.code)));
    detail.insert("message".to_string(), json!(fail.message));
    detail.extend(fail.detail);
    let detail = Value::Object(detail).to_string();
    LAST.with(|last| *last.borrow_mut() = (c_string(fail.message), c_string(detail)));
    fail.code
}

/// This thread's last error's message, valid until its next failing call.
pub(crate) fn last_message() -> *const std::ffi::c_char {
    LAST.with(|last| last.borrow().0.as_ptr())
}

/// This thread's last error's details, valid as `last_message`.
pub(crate) fn last_detail() -> *const std::ffi::c_char {
    LAST.with(|last| last.borrow().1.as_ptr())
}

/// `call` run, its error kept and its code returned; a panic caught at
/// the boundary, never unwinding into C (SPEC §177.6).
pub(crate) fn run(call: impl FnOnce() -> Result<(), Fail>) -> i32 {
    match catch_unwind(AssertUnwindSafe(call)) {
        Ok(Ok(())) => TRUNKDB_OK,
        Ok(Err(fail)) => record(fail),
        Err(panic) => {
            let why = (panic.downcast_ref::<&str>().map(|s| s.to_string()))
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "no message".to_string());
            record(Fail::new(TRUNKDB_PANIC, format!("trunkdb panicked: {why}")))
        }
    }
}
