//! Reports as JSON (SPEC §179): what a call says that is more than a
//! document, a count or an id. The shapes `trunkdb … --json` prints,
//! where it prints one (§155).

use db::{CheckReport, Compacted, FileInfo, IndexInfo, IndexStats, Stats, Summary};
use serde_json::{Value, json};

/// `check`'s: `ok` and the counts, as `trunkdb check --json`.
pub(crate) fn check(report: &CheckReport) -> Value {
    json!({
        "ok": report.is_ok(),
        "collections": report.collections,
        "documents": report.documents,
        "pages": report.pages,
        "problems": report.problems,
    })
}

/// `compact`'s: the file's pages before and after.
pub(crate) fn compacted(compacted: &Compacted) -> Value {
    json!({
        "pages_before": compacted.pages_before,
        "pages_after": compacted.pages_after,
    })
}

/// An export's or an import's: what it held, and whether it was whole.
pub(crate) fn summary(summary: &Summary) -> Value {
    json!({
        "collections": summary.collections,
        "documents": summary.documents,
        "ended": summary.ended,
    })
}

/// A collection's indexes, as `trunkdb info --json` lists them.
pub(crate) fn indexes(indexes: &[IndexInfo]) -> Value {
    let list: Vec<Value> = (indexes.iter())
        .map(|index| {
            json!({
                "fields": index.fields(),
                "unique": index.is_unique(),
                "sparse": index.is_sparse(),
            })
        })
        .collect();
    Value::Array(list)
}

fn file(info: &FileInfo) -> Value {
    let release = |release: Option<db::Release>| release.map(|r| r.to_string());
    json!({
        "format": info.format_version,
        "page_size": info.page_size,
        "pages": info.pages,
        "free_pages": info.free_pages,
        "created_by": release(info.created_by),
        "written_by": release(info.written_by),
    })
}

fn index(stats: &IndexStats) -> Value {
    json!({
        "fields": stats.fields,
        "unique": stats.unique,
        "sparse": stats.sparse,
        "entries": stats.entries,
        "pages": stats.pages,
        "depth": stats.depth,
        "key_bytes": stats.key_bytes,
    })
}

/// `stats`: the file, where its pages go, and each collection's sizes
/// and indexes (SPEC §93).
pub(crate) fn stats(stats: &Stats) -> Value {
    let pages = &stats.pages;
    let collections: Vec<Value> = (stats.collections.iter())
        .map(|c| {
            json!({
                "name": c.name,
                "documents": c.documents,
                "data_pages": c.data_pages,
                "overflow_pages": c.overflow_pages,
                "document_bytes": c.document_bytes,
                "free_bytes": c.free_bytes,
                "primary": index(&c.primary),
                "indexes": c.indexes.iter().map(index).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "file": file(&stats.file),
        "pages": {
            "header": pages.header,
            "catalog": pages.catalog,
            "free": pages.free,
            "data": pages.data,
            "overflow": pages.overflow,
            "index": pages.index,
            "unaccounted": pages.unaccounted,
        },
        "collections": collections,
        "wal_bytes": stats.wal_bytes,
        "recovered": stats.recovered.as_ref().map(|r| json!({
            "pages": r.pages,
            "in_memory": r.in_memory,
        })),
    })
}
