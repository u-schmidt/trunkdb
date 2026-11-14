//! Reports as documents (SPEC §198): what a call says that is more than
//! a document, a count or an id, in one shape for everything that hands
//! it on. The C ABI writes them as JSON (§179), the command prints them
//! with `--json` (§155), the Python bindings hand them out as dicts
//! (§197).

use crate::batch_lines::{BatchLines, OpKind};
use crate::check::{CheckReport, FileInfo};
use crate::collection::Upserted;
use crate::compact::Compacted;
use crate::document::{DocId, Document};
use crate::map::Map;
use crate::stats::{IndexStats, Stats};
use crate::{IndexInfo, Release, Summary, Written};

/// An object of `fields`, in their order.
fn object<const N: usize>(fields: [(&str, Document); N]) -> Document {
    let mut map = Map::new();
    for (name, value) in fields {
        map.insert(name.to_string(), value);
    }
    Document::Object(map)
}

/// A count as a document's integer; one past `i64` (never, for a count
/// of pages or bytes) as its largest.
fn int(n: impl TryInto<i64>) -> Document {
    Document::Int(n.try_into().unwrap_or(i64::MAX))
}

fn strings(list: &[String]) -> Document {
    Document::Array(list.iter().cloned().map(Document::String).collect())
}

fn release(release: Option<Release>) -> Document {
    release.map_or(Document::Null, |r| Document::String(r.to_string()))
}

impl CheckReport {
    /// `ok` and the counts, as `trunkdb check --json` prints them (SPEC
    /// §155, §198).
    pub fn to_document(&self) -> Document {
        object([
            ("ok", Document::Bool(self.is_ok())),
            ("collections", int(self.collections)),
            ("documents", int(self.documents)),
            ("pages", int(self.pages)),
            ("problems", strings(&self.problems)),
        ])
    }
}

impl Compacted {
    /// The file's pages before and after (SPEC §198).
    pub fn to_document(&self) -> Document {
        object([
            ("pages_before", int(self.pages_before)),
            ("pages_after", int(self.pages_after)),
        ])
    }
}

impl Summary {
    /// What an export or an import held, and whether it was whole (SPEC
    /// §198).
    pub fn to_document(&self) -> Document {
        object([
            ("collections", int(self.collections)),
            ("documents", int(self.documents)),
            ("ended", Document::Bool(self.ended)),
        ])
    }
}

impl IndexInfo {
    /// Its fields, and whether it is unique and sparse, as `trunkdb info
    /// --json` lists an index (SPEC §155, §198).
    pub fn to_document(&self) -> Document {
        let fields: Vec<String> = self.fields().iter().map(|f| f.to_string()).collect();
        object([
            ("fields", strings(&fields)),
            ("unique", Document::Bool(self.is_unique())),
            ("sparse", Document::Bool(self.is_sparse())),
        ])
    }
}

impl FileInfo {
    /// The file's format, pages and releases (SPEC §198).
    pub fn to_document(&self) -> Document {
        object([
            ("format", int(self.format_version)),
            ("page_size", int(self.page_size)),
            ("pages", int(self.pages)),
            ("free_pages", int(self.free_pages)),
            ("created_by", release(self.created_by)),
            ("written_by", release(self.written_by)),
        ])
    }
}

impl IndexStats {
    /// An index's entries, pages, depth and key bytes (SPEC §198).
    pub fn to_document(&self) -> Document {
        object([
            ("fields", strings(&self.fields)),
            ("unique", Document::Bool(self.unique)),
            ("sparse", Document::Bool(self.sparse)),
            ("entries", int(self.entries)),
            ("pages", int(self.pages)),
            ("depth", int(self.depth)),
            ("key_bytes", int(self.key_bytes)),
        ])
    }
}

impl Stats {
    /// The file, where its pages go, and each collection's sizes and
    /// indexes (SPEC §93, §198).
    pub fn to_document(&self) -> Document {
        let pages = &self.pages;
        let collections = (self.collections.iter())
            .map(|c| {
                object([
                    ("name", Document::String(c.name.clone())),
                    ("documents", int(c.documents)),
                    ("data_pages", int(c.data_pages)),
                    ("overflow_pages", int(c.overflow_pages)),
                    ("document_bytes", int(c.document_bytes)),
                    ("free_bytes", int(c.free_bytes)),
                    ("primary", c.primary.to_document()),
                    (
                        "indexes",
                        Document::Array(c.indexes.iter().map(IndexStats::to_document).collect()),
                    ),
                ])
            })
            .collect();
        object([
            ("file", self.file.to_document()),
            (
                "pages",
                object([
                    ("header", int(pages.header)),
                    ("catalog", int(pages.catalog)),
                    ("free", int(pages.free)),
                    ("data", int(pages.data)),
                    ("overflow", int(pages.overflow)),
                    ("index", int(pages.index)),
                    ("unaccounted", int(pages.unaccounted)),
                ]),
            ),
            ("collections", Document::Array(collections)),
            ("wal_bytes", self.wal_bytes.map_or(Document::Null, int)),
            (
                "recovered",
                self.recovered.as_ref().map_or(Document::Null, |r| {
                    object([
                        ("pages", int(r.pages)),
                        ("in_memory", Document::Bool(r.in_memory)),
                    ])
                }),
            ),
        ])
    }
}

impl OpKind {
    /// The op's name in a batch's line: `insert`, `update`, `delete`,
    /// `upsert`, `require`, `require_none` (SPEC §168).
    pub fn name(self) -> &'static str {
        match self {
            OpKind::Insert => "insert",
            OpKind::Update => "update",
            OpKind::Delete => "delete",
            OpKind::Upsert => "upsert",
            OpKind::Require => "require",
            OpKind::RequireNone => "require_none",
        }
    }
}

impl BatchLines {
    /// What the batch these lines made wrote (SPEC §198): `{"ops": [{"line":
    /// 1, "op": "insert", "count": 1, "id": …}, …], "total": …, "dry_run":
    /// …}`. Each op's line, what it wrote, an insert's or an upsert's id,
    /// and an upsert's `inserted`. `ids` are what `batch` handed out with
    /// it. A dry run has no ids: the next call would choose others.
    pub fn report(&self, written: &Written, ids: &[Option<DocId>], dry_run: bool) -> Document {
        let ops = (self.ops().iter().enumerate())
            .map(|(at, op)| {
                let mut entry = Map::new();
                entry.insert("line".into(), int(op.line()));
                entry.insert("op".into(), Document::String(op.kind().name().into()));
                entry.insert("count".into(), int(written.counts()[at]));
                let upserted = written.upserted(at);
                let id = ids.get(at).copied().flatten();
                if !dry_run && let Some(id) = id.or_else(|| upserted.map(|u| u.id())) {
                    // As its text, as the C ABI hands every id out.
                    entry.insert("id".into(), Document::String(id.to_string()));
                }
                if let Some(upserted) = upserted {
                    let inserted = matches!(upserted, Upserted::Inserted(_));
                    entry.insert("inserted".into(), Document::Bool(inserted));
                }
                Document::Object(entry)
            })
            .collect();
        object([
            ("ops", Document::Array(ops)),
            ("total", int(written.total())),
            ("dry_run", Document::Bool(dry_run)),
        ])
    }
}

#[cfg(test)]
mod tests {
    use crate::query::{Filter, Params};
    use crate::{BatchLines, Database, Document, IndexOptions};

    fn doc(text: &str) -> Document {
        text.parse().unwrap()
    }

    /// Each report's shape, as the C ABI hands it out and `--json`
    /// prints it: what §198 moved here, unchanged.
    #[test]
    fn reports_are_documents_of_their_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("r.trunkdb")).unwrap();
        let tasks = db.collection::<Document>("tasks");
        tasks
            .ensure_index_with(["team", "n"], IndexOptions::new().unique())
            .unwrap();
        tasks.insert(doc(r#"{"team": "a", "n": 1}"#)).unwrap();

        let check = db.check().unwrap().to_document();
        assert_eq!(
            check,
            doc(r#"{"ok": true, "collections": 1, "documents": 1, "pages": CHECKED, "problems": []}"#
                .replace("CHECKED", &db.check().unwrap().pages.to_string())
                .as_str())
        );
        assert_eq!(
            tasks.indexes().unwrap()[0].to_document(),
            doc(r#"{"fields": ["team", "n"], "unique": true, "sparse": false}"#)
        );
        let stats = db.stats().unwrap().to_document();
        let keys = |d: &Document| match d {
            Document::Object(map) => map.keys().cloned().collect::<Vec<_>>(),
            _ => panic!("an object"),
        };
        assert_eq!(
            keys(&stats),
            ["file", "pages", "collections", "wal_bytes", "recovered"]
        );
        assert_eq!(
            keys(stats.get("file").unwrap()),
            [
                "format",
                "page_size",
                "pages",
                "free_pages",
                "created_by",
                "written_by"
            ]
        );
        let Some(Document::Array(collections)) = stats.get("collections") else {
            panic!("collections");
        };
        assert_eq!(
            collections[0].get("name"),
            Some(&Document::String("tasks".into()))
        );
        assert_eq!(
            keys(collections[0].get("primary").unwrap()),
            [
                "fields",
                "unique",
                "sparse",
                "entries",
                "pages",
                "depth",
                "key_bytes"
            ]
        );
        // Pages to free: documents written and deleted again.
        for n in 100..400 {
            let big = format!(r#"{{"team": "z", "n": {n}, "pad": "{}"}}"#, "x".repeat(500));
            tasks.insert(doc(&big)).unwrap();
        }
        tasks.delete_many("n >= 100".parse().unwrap()).unwrap();
        db.checkpoint().unwrap();
        let compacted = db.compact().unwrap();
        assert_eq!(
            compacted.to_document(),
            doc(&format!(
                r#"{{"pages_before": {}, "pages_after": {}}}"#,
                compacted.pages_before, compacted.pages_after
            ))
        );
        assert_ne!(compacted.pages_before, compacted.pages_after, "told apart");
        let summary = db.export(Vec::new()).unwrap().to_document();
        assert_eq!(
            summary,
            doc(r#"{"collections": 1, "documents": 1, "ended": true}"#)
        );
    }

    /// A batch's report: each op's line, what it wrote, an insert's and
    /// an upsert's id as text, an upsert's `inserted`; no ids on a dry
    /// run.
    #[test]
    fn a_batchs_report_names_each_ops_line() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("b.trunkdb")).unwrap();
        db.collection::<Document>("tasks")
            .insert(doc(r#"{"n": 1}"#))
            .unwrap();
        let lines = BatchLines::read(
            concat!(
                r#"{"op": "insert", "collection": "tasks", "document": {"n": 2}}"#,
                "\n\n",
                r#"{"op": "upsert", "collection": "tasks", "filter": "n = 3", "update": {"$set": {"m": 1}}}"#,
                "\n",
                r#"{"op": "require", "collection": "tasks", "filter": "n = 1"}"#,
                "\n",
                r#"{"op": "require_none", "collection": "tasks", "filter": "n = 99"}"#,
            ),
            &Params::new(),
        )
        .unwrap();
        for dry_run in [true, false] {
            let (batch, ids) = lines.batch(&db).unwrap();
            let written = match dry_run {
                true => batch.dry_run(),
                false => batch.commit(),
            }
            .unwrap();
            let report = lines.report(&written, &ids, dry_run);
            let Some(Document::Array(ops)) = report.get("ops") else {
                panic!("ops");
            };
            let line = |at: usize| ops[at].get("line").cloned();
            assert_eq!(
                (line(0), line(1), line(2)),
                (
                    Some(Document::Int(1)),
                    Some(Document::Int(3)),
                    Some(Document::Int(4))
                )
            );
            let names: Vec<_> = ops.iter().map(|op| op.get("op").cloned()).collect();
            let named = |name: &str| Some(Document::String(name.into()));
            assert_eq!(
                names,
                [
                    named("insert"),
                    named("upsert"),
                    named("require"),
                    named("require_none")
                ]
            );
            assert_eq!(ops[1].get("inserted"), Some(&Document::Bool(true)));
            assert_eq!(
                ops[2].get("count"),
                Some(&Document::Int(0)),
                "a require writes nothing"
            );
            let id = ops[0].get("id");
            match dry_run {
                true => assert_eq!(id, None),
                false => {
                    let Some(Document::String(text)) = id else {
                        panic!("an id as text");
                    };
                    let id: crate::DocId = text.parse().unwrap();
                    assert_eq!(ids[0], Some(id));
                    assert!(matches!(ops[1].get("id"), Some(Document::String(_))));
                }
            }
            assert_eq!(report.get("total"), Some(&Document::Int(2)));
            assert_eq!(report.get("dry_run"), Some(&Document::Bool(dry_run)));
        }
        assert_eq!(
            db.collection::<Document>("tasks")
                .count(Filter::new())
                .unwrap(),
            3
        );
    }

    /// The value a call's texts have no parameter of, the first by name.
    #[test]
    fn a_value_no_text_takes_is_found() {
        let params = Params::new().set("a", 1).set("b", 2).set("c", 3);
        assert_eq!(params.unused(&["a", "b", "c"]), None);
        assert_eq!(params.unused(&["a", "c"]), Some("b"));
        assert_eq!(params.unused(&[]), Some("a"));
        assert_eq!(Params::new().unused(&[]), None);
    }
}
