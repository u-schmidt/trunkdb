//! A batch's ops read from JSON lines, one op a line (SPEC §168.3,
//! §175.3, §178): what `trunkdb batch` reads, and what a program that
//! speaks JSON passes, through the C ABI (§177).

use crate::query::{BindError, Filter, Params, ParseFilterError, Template, Update};
use crate::{Batch, Database, DocId, Document, Error};

/// What a line's op does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OpKind {
    /// `insert`: a document.
    Insert,
    /// `update`: operators made to what a filter finds.
    Update,
    /// `delete`: what a filter finds.
    Delete,
    /// `upsert`: operators made to the one document a filter finds, or
    /// a document inserted.
    Upsert,
    /// `require`: a precondition, at least one document matches.
    Require,
    /// `require_none`: a precondition, no document matches.
    RequireNone,
}

#[derive(Debug, Clone)]
enum Step {
    Insert(Document),
    Update(Filter, Update),
    Delete(Filter),
    Require(Filter, bool),
    Upsert(Filter, Update),
}

/// One line's op, read and checked: the collection it names and the
/// line it came from.
#[derive(Debug, Clone)]
pub struct LineOp {
    line: usize,
    collection: String,
    step: Step,
}

impl LineOp {
    /// Its line, counted from 1, blank lines among them.
    pub fn line(&self) -> usize {
        self.line
    }

    /// The collection it names.
    pub fn collection(&self) -> &str {
        &self.collection
    }

    /// What it does.
    pub fn kind(&self) -> OpKind {
        match self.step {
            Step::Insert(_) => OpKind::Insert,
            Step::Update(..) => OpKind::Update,
            Step::Delete(_) => OpKind::Delete,
            Step::Require(_, false) => OpKind::Require,
            Step::Require(_, true) => OpKind::RequireNone,
            Step::Upsert(..) => OpKind::Upsert,
        }
    }
}

/// Why a batch's lines weren't read, or an op was refused as written.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BatchLineError {
    /// A line that isn't an op as written: not JSON, no `op`, a field it
    /// doesn't take, an update that isn't operators.
    #[non_exhaustive]
    #[error("line {line}: {message}")]
    Line {
        /// The line, counted from 1.
        line: usize,
        /// What's wrong with it.
        message: String,
    },
    /// A line whose filter doesn't read.
    #[non_exhaustive]
    #[error("line {line}: filter: {error}")]
    Filter {
        /// The line, counted from 1.
        line: usize,
        /// The filter's text.
        text: String,
        /// Where and why, in the filter's text.
        error: ParseFilterError,
    },
    /// A line whose filter's parameters don't have the values they need.
    #[non_exhaustive]
    #[error("line {line}: filter: {error}")]
    Bind {
        /// The line, counted from 1.
        line: usize,
        /// The filter's text.
        text: String,
        /// Which parameter, and why.
        error: BindError,
    },
    /// A value no filter of the batch has a parameter of, the first by
    /// name: a typo, as a template's `bind` refuses one.
    #[non_exhaustive]
    #[error("a value for :{name}, which no filter of the batch has a parameter of")]
    Unused {
        /// The value's name.
        name: String,
    },
    /// An op the batch refused when it was added, as a precondition with
    /// a limit is (SPEC §162.1, §175.1).
    #[non_exhaustive]
    #[error("line {line}: {error}")]
    Refused {
        /// The line, counted from 1.
        line: usize,
        /// Why.
        error: Error,
    },
}

impl BatchLineError {
    /// The line it is about; `None` for a value no line used.
    pub fn line(&self) -> Option<usize> {
        match self {
            BatchLineError::Line { line, .. }
            | BatchLineError::Filter { line, .. }
            | BatchLineError::Bind { line, .. }
            | BatchLineError::Refused { line, .. } => Some(*line),
            BatchLineError::Unused { .. } => None,
        }
    }
}

/// A batch's ops read from JSON lines (SPEC §178), each a JSON object
/// with `op` and `collection`, and what that op takes:
///
/// ```text
/// {"op": "insert", "collection": "log", "document": {"page": "/home"}}
/// {"op": "upsert", "collection": "visits", "filter": "page = :page", "update": {"$inc": {"hits": 1}}}
/// {"op": "update", "collection": "tasks", "filter": "done = true", "update": {"$set": {"archived": true}}}
/// {"op": "delete", "collection": "tasks", "filter": "", "all": true}
/// {"op": "require", "collection": "users", "filter": "name = \"ada\""}
/// ```
///
/// Read and checked whole before anything is written; then
/// [`BatchLines::batch`] makes the [`Batch`] to commit or dry-run.
#[derive(Debug, Clone, Default)]
pub struct BatchLines {
    ops: Vec<LineOp>,
}

impl BatchLines {
    /// The ops of `text`, one a line, blank lines passed over, each
    /// filter bound to the values of `params` it has parameters for.
    /// The first line that isn't an op as written is the error, or else
    /// a value no filter has a parameter of.
    pub fn read(text: &str, params: &Params) -> Result<BatchLines, BatchLineError> {
        let mut used = Vec::new();
        let mut ops = Vec::new();
        for (at, line) in text.lines().enumerate() {
            if !line.trim().is_empty() {
                ops.push(read_line(line, at + 1, params, &mut used)?);
            }
        }
        if let Some(name) = params.names().find(|name| !used.iter().any(|u| u == name)) {
            return Err(BatchLineError::Unused {
                name: name.to_string(),
            });
        }
        Ok(BatchLines { ops })
    }

    /// The ops, in the order of their lines.
    pub fn ops(&self) -> &[LineOp] {
        &self.ops
    }

    /// How many ops.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// No ops: no line, or only blank ones.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// The ops as a batch of `db`, with each insert's new id, `None` for
    /// the other ops: an upsert's id is known at `commit`, from
    /// [`Written::upserted`](crate::Written::upserted). An op the batch
    /// refuses as written is the error, naming its line.
    pub fn batch(&self, db: &Database) -> Result<(Batch, Vec<Option<DocId>>), BatchLineError> {
        self.batch_of(db, &self.ops)
    }

    fn batch_of(
        &self,
        db: &Database,
        ops: &[LineOp],
    ) -> Result<(Batch, Vec<Option<DocId>>), BatchLineError> {
        let mut batch = db.batch();
        let mut ids = Vec::new();
        for op in ops {
            let target = db.collection::<Document>(&op.collection);
            let mut id = None;
            let added = match &op.step {
                Step::Insert(doc) => batch.insert(&target, doc.clone()).map(|new| id = Some(new)),
                Step::Update(filter, update) => batch.update_many(&target, filter.clone(), update),
                Step::Delete(filter) => {
                    batch.delete_many(&target, filter.clone());
                    Ok(())
                }
                Step::Require(filter, false) => batch.require(&target, filter.clone()),
                Step::Require(filter, true) => batch.require_none(&target, filter.clone()),
                Step::Upsert(filter, update) => {
                    batch.upsert_fields(&target, filter.clone(), update)
                }
            };
            added.map_err(|error| BatchLineError::Refused {
                line: op.line,
                error,
            })?;
            ids.push(id);
        }
        Ok((batch, ids))
    }

    /// The line of the op that `error`, from this batch's `commit` or
    /// `dry_run` on `db`, came from: a precondition's own, any other
    /// error's by dry runs of ever shorter beginnings of the batch, the
    /// first that fails ending at it. `None` if no beginning fails, as
    /// when another write came in between. Run it while nothing else
    /// writes to `db`, as a program holding the file alone (§27).
    pub fn failed_line(&self, db: &Database, error: &Error) -> Option<usize> {
        if let Error::PreconditionFailed { op, .. } = error {
            return self.ops.get(*op).map(|op| op.line);
        }
        // A beginning of `ok` ops runs; one of `fails` doesn't.
        let (mut ok, mut fails) = (0, self.ops.len());
        if fails == 0 || self.batch_of(db, &self.ops).ok()?.0.dry_run().is_ok() {
            return None;
        }
        while fails - ok > 1 {
            let mid = (ok + fails) / 2;
            match self.batch_of(db, &self.ops[..mid]).ok()?.0.dry_run() {
                Ok(_) => ok = mid,
                Err(_) => fails = mid,
            }
        }
        Some(self.ops[fails - 1].line)
    }
}

/// `text`, line `line` of a batch, as an op; the names of `params` its
/// filter took added to `used`.
fn read_line(
    text: &str,
    line: usize,
    params: &Params,
    used: &mut Vec<String>,
) -> Result<LineOp, BatchLineError> {
    let mistake = |message: String| BatchLineError::Line { line, message };
    let doc: Document = text
        .parse()
        .map_err(|e| mistake(format!("not a JSON object: {e}")))?;
    let Document::Object(mut fields) = doc else {
        return Err(mistake(
            r#"an op is a JSON object, like {"op": "delete", "collection": "tasks", "filter": "done = true"}"#
                .to_string(),
        ));
    };
    let mut take = |name: &str| fields.remove(name);
    let op = match take("op") {
        Some(Document::String(op)) => op,
        Some(_) => return Err(mistake("`op` is a string".to_string())),
        None => {
            return Err(mistake(
                "no `op`: one of insert, update, delete, upsert, require, require_none".to_string(),
            ));
        }
    };
    let collection = match take("collection") {
        Some(Document::String(name)) if !name.is_empty() => name,
        Some(_) => {
            return Err(mistake(
                "`collection` is a collection's name, a string".to_string(),
            ));
        }
        None => return Err(mistake(format!("{op}: no `collection`"))),
    };
    let filter_text = take("filter");
    let mut filter = |what: &str| -> Result<(Filter, bool), BatchLineError> {
        let text = match filter_text.clone() {
            Some(Document::String(text)) => text,
            Some(_) => {
                return Err(mistake(format!(
                    "{what}: `filter` is a filter's text, a string, \"\" for none"
                )));
            }
            None => {
                return Err(mistake(format!(
                    "{what}: no `filter`; \"\" takes every document"
                )));
            }
        };
        let template = match text.parse::<Template<Filter>>() {
            Ok(template) => template,
            Err(error) => return Err(BatchLineError::Filter { line, text, error }),
        };
        let names = template.params();
        let mine = params.only(&names);
        used.extend(mine.names().map(str::to_string));
        match template.bind(&mine) {
            Ok(parsed) => Ok((parsed, text.trim().is_empty())),
            Err(error) => Err(BatchLineError::Bind { line, text, error }),
        }
    };
    let update = |update: Option<Document>, example: &str| match update {
        Some(update) => Update::try_from(update).map_err(|e| mistake(e.to_string())),
        None => Err(mistake(format!(
            "{op}: no `update`, the operators, like {example}"
        ))),
    };
    let step = match op.as_str() {
        "insert" => match take("document") {
            Some(doc @ Document::Object(_)) => Step::Insert(doc),
            Some(_) => return Err(mistake("insert: `document` is a JSON object".to_string())),
            None => return Err(mistake("insert: no `document`".to_string())),
        },
        "update" | "delete" => {
            let (parsed, empty) = filter(&op)?;
            let all = match take("all") {
                Some(Document::Bool(all)) => all,
                None => false,
                Some(_) => return Err(mistake(format!("{op}: `all` is true or false"))),
            };
            if empty && !all {
                return Err(mistake(format!(
                    "{op}: the filter is empty, which takes every document of {collection}; add \"all\": true if that's meant"
                )));
            }
            match op.as_str() {
                "update" => Step::Update(
                    parsed,
                    update(take("update"), r#"{"$set": {"status": "done"}}"#)?,
                ),
                _ => Step::Delete(parsed),
            }
        }
        "require" | "require_none" => Step::Require(filter(&op)?.0, op == "require_none"),
        // At most one document, so an empty filter needs no `all`: the
        // collection's one document, as for settings kept in one.
        "upsert" => {
            let parsed = filter(&op)?.0;
            Step::Upsert(parsed, update(take("update"), r#"{"$inc": {"hits": 1}}"#)?)
        }
        other => {
            return Err(mistake(format!(
                "{other}: not an op; one of insert, update, delete, upsert, require, require_none"
            )));
        }
    };
    if filter_text.is_some() && matches!(step, Step::Insert(_)) {
        return Err(mistake("insert: takes no `filter`".to_string()));
    }
    if let Some(name) = fields.keys().next() {
        return Err(mistake(format!("`{name}`: not a field of an op line")));
    }
    Ok(LineOp {
        line,
        collection,
        step,
    })
}
