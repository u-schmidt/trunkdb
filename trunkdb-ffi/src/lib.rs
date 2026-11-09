//! trunkdb's C ABI (SPEC §177, §179): the library as plain C functions,
//! for every language that calls C. `include/trunkdb.h` declares them;
//! its opening comment has the rules every function keeps.
//!
//! Every function takes raw pointers from C, so every one is `unsafe` to
//! call: the rules in the header are its safety contract, the same for
//! all of them, and not repeated on each.

#![warn(missing_docs)]
#![allow(non_camel_case_types, clippy::missing_safety_doc)]

mod error;
mod handle;
mod report;
mod text;

#[cfg(test)]
mod tests;

use db::query::{Aggregate, Filter};
use db::{BatchLineError, BatchLines, Database, DocId, Document, OpKind, Snapshot, Upserted};
use error::{Fail, run};
use handle::{CURSOR, DATABASE, SNAPSHOT, close, hand_out, value};
use serde_json::{Value, json};
use std::ffi::c_char;
use std::sync::Mutex;
use text::{optional, owned, text};

/// The version of these functions: a library whose `trunkdb_abi_version`
/// differs from the header's is not the one the program was built for.
pub const TRUNKDB_ABI_VERSION: u32 = 1;

/// The call did what it says.
pub const TRUNKDB_OK: i32 = 0;
/// A pointer or text that isn't what the call takes: null where a value
/// is needed, text that isn't UTF-8, a handle of another kind or closed;
/// or what the library refuses as asked, as an index on fields that have
/// one of other options, or a batch's precondition with a limit.
pub const TRUNKDB_INVALID_ARGUMENT: i32 = 1;
/// A text that doesn't read: a filter, an aggregate, a document, an
/// update, parameters, an id, options or a batch's line.
pub const TRUNKDB_PARSE: i32 = 2;
/// The file system's error, or a file that is damaged.
pub const TRUNKDB_IO: i32 = 3;
/// A file that isn't trunkdb's, or of a format this build doesn't read.
pub const TRUNKDB_FORMAT: i32 = 4;
/// A write to a database opened read-only.
pub const TRUNKDB_READ_ONLY: i32 = 5;
/// A failed write left the database unusable until it is opened again.
pub const TRUNKDB_POISONED: i32 = 6;
/// An insert of an id the collection has.
pub const TRUNKDB_DUPLICATE_ID: i32 = 7;
/// A unique index refused a value another document has.
pub const TRUNKDB_DUPLICATE_VALUE: i32 = 8;
/// A batch's op on an id the collection doesn't have.
pub const TRUNKDB_NOT_FOUND: i32 = 9;
/// An upsert whose filter matches more than one document.
pub const TRUNKDB_MULTIPLE_MATCHES: i32 = 10;
/// An update's operator that can't be made to a document.
pub const TRUNKDB_UPDATE: i32 = 11;
/// A batch's precondition failed.
pub const TRUNKDB_PRECONDITION_FAILED: i32 = 12;
/// A document that isn't an object.
pub const TRUNKDB_NOT_AN_OBJECT: i32 = 13;
/// An index hint that can't be followed.
pub const TRUNKDB_HINT: i32 = 14;
/// A line of an import that isn't what an export writes.
pub const TRUNKDB_IMPORT: i32 = 15;
/// A compaction while a snapshot is open.
pub const TRUNKDB_SNAPSHOT_OPEN: i32 = 16;
/// A snapshot kept past the memory snapshots may take.
pub const TRUNKDB_SNAPSHOT_TOO_OLD: i32 = 17;
/// A panic inside trunkdb, caught at the boundary.
pub const TRUNKDB_PANIC: i32 = 18;
/// An error of a kind added after this header.
pub const TRUNKDB_OTHER: i32 = 19;

/// An open database: from `trunkdb_open`, closed by `trunkdb_close`. Any
/// number of threads may use one at once.
pub struct trunkdb_db {
    _opaque: [u8; 0],
}

/// The database as it was at one commit: from `trunkdb_snapshot_open`,
/// closed by `trunkdb_snapshot_close`. Any number of threads may read it
/// at once.
pub struct trunkdb_snapshot {
    _opaque: [u8; 0],
}

/// A read handed out a document at a time: from `trunkdb_cursor_open`,
/// closed by `trunkdb_cursor_close`.
pub struct trunkdb_cursor {
    _opaque: [u8; 0],
}

// ---- Handles and pointers ----

/// The database behind `db`.
unsafe fn database<'a>(db: *const trunkdb_db) -> Result<&'a Database, Fail> {
    // SAFETY: by the rules.
    unsafe { value::<Database, _>(db, DATABASE, "db") }
}

/// The snapshot behind `snapshot`, or `None` for null.
unsafe fn snapshot<'a>(snapshot: *const trunkdb_snapshot) -> Result<Option<&'a Snapshot>, Fail> {
    if snapshot.is_null() {
        return Ok(None);
    }
    // SAFETY: by the rules.
    unsafe { value::<Snapshot, _>(snapshot, SNAPSHOT, "snapshot").map(Some) }
}

/// An out parameter, set to `empty` before anything else, so an error
/// leaves it null or 0.
unsafe fn out<'a, T>(pointer: *mut T, empty: T, what: &str) -> Result<&'a mut T, Fail> {
    if pointer.is_null() {
        return Err(Fail::argument(format!("{what} is null")));
    }
    // SAFETY: by the rules, a place to write a `T`.
    unsafe {
        pointer.write(empty);
        Ok(&mut *pointer)
    }
}

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
}

/// `collection` of `db`, or of `at` where it isn't null.
unsafe fn reader(
    db: *const trunkdb_db,
    at: *const trunkdb_snapshot,
    collection: *const c_char,
) -> Result<Reader, Fail> {
    // SAFETY: by the rules.
    unsafe {
        let db = database(db)?;
        let name = text(collection, "collection")?;
        Ok(match snapshot(at)? {
            Some(snapshot) => Reader::At(snapshot.collection(name)),
            None => Reader::Live(db.collection(name)),
        })
    }
}

/// `collection` of `db`, to write to.
unsafe fn writer(
    db: *const trunkdb_db,
    collection: *const c_char,
) -> Result<db::Collection<Document>, Fail> {
    // SAFETY: by the rules.
    unsafe { Ok(database(db)?.collection(text(collection, "collection")?)) }
}

/// A file system error about `path`.
fn io(path: &str, e: std::io::Error) -> Fail {
    Fail::new(TRUNKDB_IO, format!("{path}: {e}")).with("io", format!("{:?}", e.kind()))
}

/// JSON's text, to hand out.
fn json_out(value: Value) -> *mut c_char {
    owned(value.to_string())
}

// ---- General ----

/// The version of these functions the library was built with:
/// `TRUNKDB_ABI_VERSION` of its header.
#[unsafe(no_mangle)]
pub extern "C" fn trunkdb_abi_version() -> u32 {
    TRUNKDB_ABI_VERSION
}

/// trunkdb's release, "0.37.0": owned by the library, never freed.
#[unsafe(no_mangle)]
pub extern "C" fn trunkdb_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

/// The message of this thread's last error, as text: owned by the
/// library, valid until the thread's next call that fails. Empty before
/// any has.
#[unsafe(no_mangle)]
pub extern "C" fn trunkdb_error_message() -> *const c_char {
    error::last_message()
}

/// This thread's last error as JSON: `code`, `kind`, `message`, and what
/// else the error knows, such as `collection`, `field`, `id`, `line` or
/// `offset`. Owned by the library, valid as `trunkdb_error_message`.
#[unsafe(no_mangle)]
pub extern "C" fn trunkdb_error_detail() -> *const c_char {
    error::last_detail()
}

/// Gives back a string a call handed out. Null does nothing. Never pass
/// one to C's `free`, nor a string the library didn't hand out.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_string_free(text: *mut c_char) {
    if !text.is_null() {
        // SAFETY: by the rules, made by `CString::into_raw`.
        drop(unsafe { std::ffi::CString::from_raw(text) });
    }
}

// ---- The database ----

/// Opens the database at `path`, creating it if it isn't there.
/// `options`, JSON or null: `read_only` (true or false), `cache_size`,
/// `snapshot_memory` (bytes), `checkpoint_pages`, `checkpoint_wal_bytes`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_open(
    path: *const c_char,
    options: *const c_char,
    db: *mut *mut trunkdb_db,
) -> i32 {
    run(|| unsafe {
        let db = out(db, std::ptr::null_mut(), "db")?;
        let path = text(path, "path")?;
        let given = text::options(
            optional(options, "options")?,
            &[
                "read_only",
                "cache_size",
                "snapshot_memory",
                "checkpoint_pages",
                "checkpoint_wal_bytes",
            ],
        )?;
        let mut open =
            db::OpenOptions::default().read_only(text::flag_option(&given, "read_only")?);
        let size = |n: u64| usize::try_from(n).unwrap_or(usize::MAX);
        if let Some(n) = text::count_option(&given, "cache_size")? {
            open = open.cache_size(size(n));
        }
        if let Some(n) = text::count_option(&given, "snapshot_memory")? {
            open = open.snapshot_memory(size(n));
        }
        if let Some(n) = text::count_option(&given, "checkpoint_pages")? {
            open = open.checkpoint_pages(size(n));
        }
        if let Some(n) = text::count_option(&given, "checkpoint_wal_bytes")? {
            open = open.checkpoint_wal_bytes(n);
        }
        let opened = Database::open_with(path, open)?;
        *db = hand_out(DATABASE, opened);
        Ok(())
    })
}

/// Closes `db`. The file closes once its snapshots and cursors are closed
/// too. Null does nothing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_close(db: *mut trunkdb_db) -> i32 {
    run(|| unsafe { close::<Database, _>(db, DATABASE, "db") })
}

/// Writes what the log holds into the file, and empties the log.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_checkpoint(db: *const trunkdb_db) -> i32 {
    run(|| unsafe { Ok(database(db)?.checkpoint()?) })
}

/// Rebuilds the file without its free pages: `{"pages_before": …,
/// "pages_after": …}`. Fails while a snapshot is open.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_compact(db: *const trunkdb_db, report: *mut *mut c_char) -> i32 {
    run(|| unsafe {
        let report = out(report, std::ptr::null_mut(), "report")?;
        *report = json_out(report::compacted(&database(db)?.compact()?));
        Ok(())
    })
}

/// Reads the whole file and checks it: `{"ok": …, "collections": …,
/// "documents": …, "pages": …, "problems": […]}`. Problems found are
/// not an error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_check(db: *const trunkdb_db, report: *mut *mut c_char) -> i32 {
    run(|| unsafe {
        let report = out(report, std::ptr::null_mut(), "report")?;
        *report = json_out(report::check(&database(db)?.check()?));
        Ok(())
    })
}

/// Where the file's space goes: the file, its pages by use, and each
/// collection's documents, sizes and indexes, as JSON.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_stats(db: *const trunkdb_db, report: *mut *mut c_char) -> i32 {
    run(|| unsafe {
        let report = out(report, std::ptr::null_mut(), "report")?;
        *report = json_out(report::stats(&database(db)?.stats()?));
        Ok(())
    })
}

/// The collections' names, a JSON array in name order.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_collections(
    db: *const trunkdb_db,
    names: *mut *mut c_char,
) -> i32 {
    run(|| unsafe {
        let names = out(names, std::ptr::null_mut(), "names")?;
        *names = json_out(json!(database(db)?.collections()?));
        Ok(())
    })
}

/// Drops a collection, its documents and indexes; `dropped` says whether
/// it was there.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_drop_collection(
    db: *const trunkdb_db,
    collection: *const c_char,
    dropped: *mut bool,
) -> i32 {
    run(|| unsafe {
        let dropped = out(dropped, false, "dropped")?;
        *dropped = database(db)?.drop_collection(text(collection, "collection")?)?;
        Ok(())
    })
}

/// Exports every collection to the file at `path` as JSON lines, written
/// beside it and renamed over it once complete: `{"collections": …,
/// "documents": …, "ended": true}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_export(
    db: *const trunkdb_db,
    path: *const c_char,
    summary: *mut *mut c_char,
) -> i32 {
    run(|| unsafe {
        let summary = out(summary, std::ptr::null_mut(), "summary")?;
        let path = text(path, "path")?;
        let done = database(db)?.export_to_file(path).map_err(|e| match e {
            db::Error::Io(e) if e.kind() != std::io::ErrorKind::InvalidInput => io(path, e),
            e => Fail::from(e),
        })?;
        *summary = json_out(report::summary(&done));
        Ok(())
    })
}

/// Imports an export from the file at `path`, as one batch. `options`,
/// JSON or null: `max_line_bytes`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_import(
    db: *const trunkdb_db,
    path: *const c_char,
    options: *const c_char,
    summary: *mut *mut c_char,
) -> i32 {
    run(|| unsafe {
        let summary = out(summary, std::ptr::null_mut(), "summary")?;
        let db = database(db)?;
        let path = text(path, "path")?;
        let given = text::options(optional(options, "options")?, &["max_line_bytes"])?;
        let mut import = db::ImportOptions::default();
        if let Some(n) = text::count_option(&given, "max_line_bytes")? {
            import = import.max_line_bytes(usize::try_from(n).unwrap_or(usize::MAX));
        }
        let file = std::fs::File::open(path).map_err(|e| io(path, e))?;
        let done = db.import_with(std::io::BufReader::new(file), import)?;
        *summary = json_out(report::summary(&done));
        Ok(())
    })
}

// ---- Indexes ----

/// An index's fields: a name, or a JSON array of names.
fn index_fields(text: &str) -> Result<Vec<String>, Fail> {
    let bad = || {
        Fail::parse(
            "fields",
            r#"a field's name, or a JSON array of them, like ["team", "email"]"#,
        )
    };
    match serde_json::from_str::<Value>(text) {
        Ok(Value::String(name)) => Ok(vec![name]),
        Ok(Value::Array(names)) => (names.into_iter())
            .map(|name| match name {
                Value::String(name) => Ok(name),
                _ => Err(bad()),
            })
            .collect(),
        Ok(_) => Err(bad()),
        // A bare name, as `status`.
        Err(_) if !text.trim().is_empty() && !text.trim_start().starts_with(['[', '{', '"']) => {
            Ok(vec![text.to_string()])
        }
        Err(_) => Err(bad()),
    }
}

/// Makes an index on `fields` unless there is one; `created` says
/// whether this call made it. `options`, JSON or null: `unique`,
/// `sparse`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_index_ensure(
    db: *const trunkdb_db,
    collection: *const c_char,
    fields: *const c_char,
    options: *const c_char,
    created: *mut bool,
) -> i32 {
    run(|| unsafe {
        let created = out(created, false, "created")?;
        let target = writer(db, collection)?;
        let fields = index_fields(text(fields, "fields")?)?;
        let given = text::options(optional(options, "options")?, &["unique", "sparse"])?;
        let mut index = db::IndexOptions::new();
        if text::flag_option(&given, "unique")? {
            index = index.unique();
        }
        if text::flag_option(&given, "sparse")? {
            index = index.sparse();
        }
        *created = target.ensure_index_with(fields, index)?;
        Ok(())
    })
}

/// Drops the index on `fields`; `dropped` says whether there was one.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_index_drop(
    db: *const trunkdb_db,
    collection: *const c_char,
    fields: *const c_char,
    dropped: *mut bool,
) -> i32 {
    run(|| unsafe {
        let dropped = out(dropped, false, "dropped")?;
        let target = writer(db, collection)?;
        *dropped = target.drop_index(index_fields(text(fields, "fields")?)?)?;
        Ok(())
    })
}

/// A collection's indexes: a JSON array of `{"fields": […], "unique": …,
/// "sparse": …}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_indexes(
    db: *const trunkdb_db,
    collection: *const c_char,
    indexes: *mut *mut c_char,
) -> i32 {
    run(|| unsafe {
        let indexes = out(indexes, std::ptr::null_mut(), "indexes")?;
        *indexes = json_out(report::indexes(&writer(db, collection)?.indexes()?));
        Ok(())
    })
}

// ---- Reads: of the database now, or of a snapshot where it isn't null ----

/// The document with `id`, as JSON; null if there is none.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_get(
    db: *const trunkdb_db,
    snapshot: *const trunkdb_snapshot,
    collection: *const c_char,
    id: *const c_char,
    document: *mut *mut c_char,
) -> i32 {
    run(|| unsafe {
        let document = out(document, std::ptr::null_mut(), "document")?;
        let reader = reader(db, snapshot, collection)?;
        if let Some(found) = reader.get(&text::id(text(id, "id")?)?)? {
            *document = owned(found.to_string());
        }
        Ok(())
    })
}

/// What `filter` finds, a JSON array of documents. `filter` is a filter's
/// text, null or empty for every document; `params` its parameters'
/// values as a JSON object, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_find(
    db: *const trunkdb_db,
    snapshot: *const trunkdb_snapshot,
    collection: *const c_char,
    filter: *const c_char,
    params: *const c_char,
    documents: *mut *mut c_char,
) -> i32 {
    run(|| unsafe {
        let documents = out(documents, std::ptr::null_mut(), "documents")?;
        let reader = reader(db, snapshot, collection)?;
        let params = text::params(optional(params, "params")?)?;
        let filter = text::filter(optional(filter, "filter")?, &params, false)?;
        *documents = owned(Document::Array(reader.find(filter)?).to_string());
        Ok(())
    })
}

/// The first document `filter` finds, as JSON; null if none.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_find_one(
    db: *const trunkdb_db,
    snapshot: *const trunkdb_snapshot,
    collection: *const c_char,
    filter: *const c_char,
    params: *const c_char,
    document: *mut *mut c_char,
) -> i32 {
    run(|| unsafe {
        let document = out(document, std::ptr::null_mut(), "document")?;
        let reader = reader(db, snapshot, collection)?;
        let params = text::params(optional(params, "params")?)?;
        let filter = text::filter(optional(filter, "filter")?, &params, false)?;
        if let Some(found) = reader.find_one(filter)? {
            *document = owned(found.to_string());
        }
        Ok(())
    })
}

/// How many documents `filter` finds.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_count(
    db: *const trunkdb_db,
    snapshot: *const trunkdb_snapshot,
    collection: *const c_char,
    filter: *const c_char,
    params: *const c_char,
    count: *mut u64,
) -> i32 {
    run(|| unsafe {
        let count = out(count, 0, "count")?;
        let reader = reader(db, snapshot, collection)?;
        let params = text::params(optional(params, "params")?)?;
        let filter = text::filter(optional(filter, "filter")?, &params, false)?;
        *count = reader.count(filter)? as u64;
        Ok(())
    })
}

/// An aggregate's rows over what `filter` finds, a JSON array:
/// `aggregate` as text, `count() as n group by team`. `params` holds
/// the values of both texts' parameters, each taking its own.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_aggregate(
    db: *const trunkdb_db,
    snapshot: *const trunkdb_snapshot,
    collection: *const c_char,
    filter: *const c_char,
    aggregate: *const c_char,
    params: *const c_char,
    rows: *mut *mut c_char,
) -> i32 {
    run(|| unsafe {
        let rows = out(rows, std::ptr::null_mut(), "rows")?;
        let reader = reader(db, snapshot, collection)?;
        let params = text::params(optional(params, "params")?)?;
        let filter_text = optional(filter, "filter")?;
        let aggregate_text = text(aggregate, "aggregate")?;
        let filter = text::filter(filter_text, &params, true)?;
        let aggregate = text::aggregate(aggregate_text, &params, true)?;
        let filter_names = text::names_in::<Filter>(filter_text.unwrap_or(""));
        let aggregate_names = text::names_in::<Aggregate>(aggregate_text);
        let filter_names: Vec<&str> = filter_names.iter().map(String::as_str).collect();
        let aggregate_names: Vec<&str> = aggregate_names.iter().map(String::as_str).collect();
        text::unused(&params, &[&filter_names, &aggregate_names])?;
        *rows = owned(Document::Array(reader.aggregate(filter, &aggregate)?).to_string());
        Ok(())
    })
}

/// Runs `filter` and says how: its plan, what it read and how much, as
/// the lines `trunkdb find --explain` prints.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_explain(
    db: *const trunkdb_db,
    snapshot: *const trunkdb_snapshot,
    collection: *const c_char,
    filter: *const c_char,
    params: *const c_char,
    explained: *mut *mut c_char,
) -> i32 {
    run(|| unsafe {
        let explained = out(explained, std::ptr::null_mut(), "explained")?;
        let reader = reader(db, snapshot, collection)?;
        let params = text::params(optional(params, "params")?)?;
        let filter = text::filter(optional(filter, "filter")?, &params, false)?;
        *explained = owned(reader.explain(filter)?.to_string());
        Ok(())
    })
}

// ---- Writes ----

/// Inserts a document, a JSON object; `id` is its id, the one its `_id`
/// gives or a new one.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_insert(
    db: *const trunkdb_db,
    collection: *const c_char,
    document: *const c_char,
    id: *mut *mut c_char,
) -> i32 {
    run(|| unsafe {
        let id = out(id, std::ptr::null_mut(), "id")?;
        let target = writer(db, collection)?;
        let document = text::document(text(document, "document")?)?;
        *id = owned(target.insert(document)?.to_string());
        Ok(())
    })
}

/// Replaces the document with `id` by `document`, whole; `found` says
/// whether there was one. None is inserted.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_replace(
    db: *const trunkdb_db,
    collection: *const c_char,
    id: *const c_char,
    document: *const c_char,
    found: *mut bool,
) -> i32 {
    run(|| unsafe {
        let found = out(found, false, "found")?;
        let target = writer(db, collection)?;
        let id = text::id(text(id, "id")?)?;
        let document = text::document(text(document, "document")?)?;
        *found = target.update(&id, document)?;
        Ok(())
    })
}

/// Deletes the document with `id`; `found` says whether there was one.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_delete(
    db: *const trunkdb_db,
    collection: *const c_char,
    id: *const c_char,
    found: *mut bool,
) -> i32 {
    run(|| unsafe {
        let found = out(found, false, "found")?;
        let target = writer(db, collection)?;
        *found = target.delete(&text::id(text(id, "id")?)?)?;
        Ok(())
    })
}

/// Makes `update`, MongoDB's operators as JSON, `{"$set": {"done":
/// true}}`, to what `filter` finds; `count` is how many it changed. Null
/// or empty `filter` is every document.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_update(
    db: *const trunkdb_db,
    collection: *const c_char,
    filter: *const c_char,
    params: *const c_char,
    update: *const c_char,
    count: *mut u64,
) -> i32 {
    run(|| unsafe {
        let count = out(count, 0, "count")?;
        let target = writer(db, collection)?;
        let params = text::params(optional(params, "params")?)?;
        let filter = text::filter(optional(filter, "filter")?, &params, false)?;
        let update = text::update(text(update, "update")?)?;
        *count = target.update_fields(filter, &update)? as u64;
        Ok(())
    })
}

/// Deletes what `filter` finds; `count` is how many. Null or empty
/// `filter` is every document.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_delete_many(
    db: *const trunkdb_db,
    collection: *const c_char,
    filter: *const c_char,
    params: *const c_char,
    count: *mut u64,
) -> i32 {
    run(|| unsafe {
        let count = out(count, 0, "count")?;
        let target = writer(db, collection)?;
        let params = text::params(optional(params, "params")?)?;
        let filter = text::filter(optional(filter, "filter")?, &params, false)?;
        *count = target.delete_many(filter)? as u64;
        Ok(())
    })
}

/// Makes `update` to the one document `filter` finds, or inserts one
/// made of the filter's `=`s and then `update`: `id` is the document's,
/// `inserted` whether it was made. Several matches fail.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_upsert(
    db: *const trunkdb_db,
    collection: *const c_char,
    filter: *const c_char,
    params: *const c_char,
    update: *const c_char,
    id: *mut *mut c_char,
    inserted: *mut bool,
) -> i32 {
    run(|| unsafe {
        let id = out(id, std::ptr::null_mut(), "id")?;
        let inserted = out(inserted, false, "inserted")?;
        let target = writer(db, collection)?;
        let params = text::params(optional(params, "params")?)?;
        let filter = text::filter(optional(filter, "filter")?, &params, false)?;
        let update = text::update(text(update, "update")?)?;
        let done = target.upsert_fields(filter, &update)?;
        *inserted = matches!(done, Upserted::Inserted(_));
        *id = owned(done.id().to_string());
        Ok(())
    })
}

// ---- A batch ----

/// A batch's line that isn't an op, or an op it refused: its message
/// names the line, and so do the details.
fn line_fail(error: BatchLineError) -> Fail {
    let line = error.line();
    let message = error.to_string();
    let fail = match error {
        BatchLineError::Refused { error, .. } => Fail::from(error),
        BatchLineError::Filter { error, .. } => {
            Fail::parse("batch", &message).with("offset", error.offset())
        }
        BatchLineError::Bind { error, .. } => {
            let fail = Fail::parse("batch", &message).with("parameter", error.name());
            match error.offset() {
                Some(offset) => fail.with("offset", offset),
                None => fail,
            }
        }
        BatchLineError::Unused { name, .. } => {
            Fail::parse("batch", &message).with("parameter", name)
        }
        _ => Fail::parse("batch", &message),
    };
    match line {
        Some(line) => fail.with("line", line),
        None => fail,
    }
}

fn op_name(kind: OpKind) -> &'static str {
    match kind {
        OpKind::Insert => "insert",
        OpKind::Update => "update",
        OpKind::Delete => "delete",
        OpKind::Upsert => "upsert",
        OpKind::Require => "require",
        OpKind::RequireNone => "require_none",
        _ => "op",
    }
}

/// Runs a batch of ops, one JSON object a line, as `trunkdb batch` reads
/// them, all committed or none; with `dry_run`, each run and rolled
/// back. `params` holds the values for every line's filter. `written`
/// is `{"ops": [{"line": 1, "op": "insert", "count": 1, "id": …}, …],
/// "total": …, "dry_run": …}`: each op's line, what it wrote, an
/// insert's or an upsert's id (not on a dry run) and an upsert's
/// `inserted`. A failure's details name its `line`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_batch(
    db: *const trunkdb_db,
    lines: *const c_char,
    params: *const c_char,
    dry_run: bool,
    written: *mut *mut c_char,
) -> i32 {
    run(|| unsafe {
        let written = out(written, std::ptr::null_mut(), "written")?;
        let db = database(db)?;
        let params = text::params(optional(params, "params")?)?;
        let ops = BatchLines::read(text(lines, "lines")?, &params).map_err(line_fail)?;
        let (batch, ids) = ops.batch(db).map_err(line_fail)?;
        let outcome = match dry_run {
            true => batch.dry_run(),
            false => batch.commit(),
        };
        let done = outcome.map_err(|e| {
            let line = ops.failed_line(db, &e);
            let fail = Fail::from(e);
            match line {
                Some(line) => fail.with("line", line),
                None => fail,
            }
        })?;
        let mut list = Vec::new();
        for (at, op) in ops.ops().iter().enumerate() {
            let mut entry = serde_json::Map::new();
            entry.insert("line".into(), json!(op.line()));
            entry.insert("op".into(), json!(op_name(op.kind())));
            entry.insert("count".into(), json!(done.counts()[at]));
            let upserted = done.upserted(at);
            if !dry_run && let Some(id) = ids[at].or_else(|| upserted.map(|u| u.id())) {
                entry.insert("id".into(), json!(id.to_string()));
            }
            if let Some(upserted) = upserted {
                entry.insert(
                    "inserted".into(),
                    json!(matches!(upserted, Upserted::Inserted(_))),
                );
            }
            list.push(Value::Object(entry));
        }
        *written = json_out(json!({"ops": list, "total": done.total(), "dry_run": dry_run}));
        Ok(())
    })
}

// ---- Snapshots ----

/// The database as it is now, kept for reads however it is written to
/// after: pass it to a read instead of null. Close it soon; while open,
/// it keeps what was written since in memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_snapshot_open(
    db: *const trunkdb_db,
    snapshot: *mut *mut trunkdb_snapshot,
) -> i32 {
    run(|| unsafe {
        let snapshot = out(snapshot, std::ptr::null_mut(), "snapshot")?;
        *snapshot = hand_out(SNAPSHOT, database(db)?.snapshot()?);
        Ok(())
    })
}

/// Closes a snapshot. Null does nothing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_snapshot_close(snapshot: *mut trunkdb_snapshot) -> i32 {
    run(|| unsafe { close::<Snapshot, _>(snapshot, SNAPSHOT, "snapshot") })
}

// ---- Cursors ----

/// What `filter` finds, handed out by `trunkdb_cursor_next` a document
/// at a time, of the database as it was when opened.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_cursor_open(
    db: *const trunkdb_db,
    snapshot: *const trunkdb_snapshot,
    collection: *const c_char,
    filter: *const c_char,
    params: *const c_char,
    cursor: *mut *mut trunkdb_cursor,
) -> i32 {
    run(|| unsafe {
        let cursor = out(cursor, std::ptr::null_mut(), "cursor")?;
        let reader = reader(db, snapshot, collection)?;
        let params = text::params(optional(params, "params")?)?;
        let filter = text::filter(optional(filter, "filter")?, &params, false)?;
        *cursor = hand_out(CURSOR, Mutex::new(reader.cursor(filter)?));
        Ok(())
    })
}

/// The cursor's next document, as JSON; null at the end.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_cursor_next(
    cursor: *const trunkdb_cursor,
    document: *mut *mut c_char,
) -> i32 {
    run(|| unsafe {
        let document = out(document, std::ptr::null_mut(), "document")?;
        let cursor = value::<Mutex<db::Cursor<Document>>, _>(cursor, CURSOR, "cursor")?;
        let next = cursor.lock().unwrap_or_else(|e| e.into_inner()).next();
        if let Some(found) = next.transpose()? {
            *document = owned(found.to_string());
        }
        Ok(())
    })
}

/// Closes a cursor. Null does nothing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trunkdb_cursor_close(cursor: *mut trunkdb_cursor) -> i32 {
    run(|| unsafe { close::<Mutex<db::Cursor<Document>>, _>(cursor, CURSOR, "cursor") })
}
