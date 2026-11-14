//! Live handles (SPEC §194): a subscription, a live query or a live
//! aggregate behind one handle, polled with a timeout, each event JSON.

use crate::TRUNKDB_OTHER;
use crate::error::Fail;
use db::{Change, Changes, Commit, DocId, Document, LiveAggregate, LiveQuery, LiveRow};
use serde_json::{Value, json};
use std::time::Duration;

/// What a live handle holds.
pub(crate) enum Live {
    Changes(Changes),
    Query(LiveQuery<Document>),
    Aggregate(LiveAggregate),
}

impl Live {
    /// The next event as JSON, waiting as `timeout_ms` says; `None` if
    /// none came, or it has ended. The error that ends a live query or
    /// aggregate, once.
    pub(crate) fn next(&self, timeout_ms: i64) -> Result<Option<String>, Fail> {
        Ok(match self {
            Live::Changes(c) => wait(
                timeout_ms,
                || c.recv(),
                || c.try_recv(),
                |t| c.recv_timeout(t),
            )
            .map(commit),
            Live::Query(q) => {
                match wait(
                    timeout_ms,
                    || q.recv(),
                    || q.try_recv(),
                    |t| q.recv_timeout(t),
                ) {
                    Some(event) => Some(live_event(event?)?),
                    None => None,
                }
            }
            Live::Aggregate(a) => {
                match wait(
                    timeout_ms,
                    || a.recv(),
                    || a.try_recv(),
                    |t| a.recv_timeout(t),
                ) {
                    Some(event) => Some(row_event(event?)?),
                    None => None,
                }
            }
        })
    }

    /// Whether it has ended, every event read (SPEC §194).
    pub(crate) fn has_ended(&self) -> bool {
        match self {
            Live::Changes(c) => c.has_ended(),
            Live::Query(q) => q.has_ended(),
            Live::Aggregate(a) => a.has_ended(),
        }
    }
}

/// A receive as `timeout_ms` asks: until one comes where it is negative,
/// without waiting where it is 0.
fn wait<M>(
    timeout_ms: i64,
    until: impl FnOnce() -> Option<M>,
    now: impl FnOnce() -> Option<M>,
    within: impl FnOnce(Duration) -> Option<M>,
) -> Option<M> {
    match u64::try_from(timeout_ms) {
        Err(_) => until(),
        Ok(0) => now(),
        Ok(ms) => within(Duration::from_millis(ms)),
    }
}

/// An event of a kind added to trunkdb after this code: never, as it is
/// built with the library it calls.
fn unknown() -> Fail {
    Fail::new(TRUNKDB_OTHER, "an event of a kind this library can't write")
}

/// An id as a JSON string.
fn id(id: DocId) -> String {
    Value::String(id.to_string()).to_string()
}

/// A place in a sorted list, or null.
fn place(place: Option<usize>) -> String {
    place.map_or_else(|| "null".to_string(), |n| n.to_string())
}

/// A document as an export writes it, or null.
fn doc(doc: Option<&Document>) -> String {
    doc.map_or_else(|| "null".to_string(), Document::to_string)
}

fn commit(commit: Commit) -> String {
    let changes: Vec<Value> = (commit.changes.iter())
        .map(|change| {
            let kind = match change {
                Change::Inserted { .. } => "inserted",
                Change::Updated { .. } => "updated",
                Change::Deleted { .. } => "deleted",
                Change::Dropped { .. } => "dropped",
                _ => "other",
            };
            let mut entry = json!({"change": kind, "collection": change.collection()});
            if let Some(id) = change.id() {
                entry["id"] = json!(id.to_string());
            }
            entry
        })
        .collect();
    json!({"event": "commit", "changes": changes}).to_string()
}

fn live_event(event: db::Live<Document>) -> Result<String, Fail> {
    Ok(match event {
        db::Live::Entered {
            id: i, doc: d, at, ..
        } => format!(
            r#"{{"event":"entered","id":{},"doc":{},"at":{}}}"#,
            id(i),
            doc(Some(&d)),
            place(at)
        ),
        db::Live::Left {
            id: i,
            doc: d,
            from,
            ..
        } => format!(
            r#"{{"event":"left","id":{},"doc":{},"from":{}}}"#,
            id(i),
            doc(d.as_ref()),
            place(from)
        ),
        db::Live::Changed {
            id: i,
            doc: d,
            from,
            at,
            ..
        } => format!(
            r#"{{"event":"changed","id":{},"doc":{},"from":{},"at":{}}}"#,
            id(i),
            doc(Some(&d)),
            place(from),
            place(at)
        ),
        _ => return Err(unknown()),
    })
}

fn row_event(event: LiveRow) -> Result<String, Fail> {
    let (kind, group, row) = match &event {
        LiveRow::Entered { group, row, .. } => ("entered", group, Some(row)),
        LiveRow::Left { group, row, .. } => ("left", group, row.as_ref()),
        LiveRow::Changed { group, row, .. } => ("changed", group, Some(row)),
        _ => return Err(unknown()),
    };
    Ok(format!(
        r#"{{"event":"{kind}","group":{},"row":{}}}"#,
        doc(Some(group)),
        doc(row)
    ))
}
