//! Backups a user can trust (SPEC §165): an export ends with a line
//! saying how much it holds, an import of one checks it, and
//! `verify_export` restores an export into a file of its own to find out
//! whether it restores whole.

use std::path::Path;
use trunkdb::{Database, Document, Error, IndexOptions, Summary, verify_export};

fn open(dir: &Path, name: &str) -> Database {
    Database::open(dir.join(name)).unwrap()
}

/// Three collections, one of them empty, and a unique index.
fn sample(db: &Database) {
    let users = db.collection::<Document>("users");
    for name in ["Ada", "Bob", "Cy"] {
        users
            .insert(format!(r#"{{"name": "{name}"}}"#).parse().unwrap())
            .unwrap();
    }
    users
        .ensure_index_with("name", IndexOptions::new().unique())
        .unwrap();
    let pings = db.collection::<Document>("pings");
    for n in 0..5 {
        pings
            .insert(format!(r#"{{"n": {n}}}"#).parse().unwrap())
            .unwrap();
    }
    db.collection::<Document>("empty")
        .ensure_index("x")
        .unwrap();
}

fn export(db: &Database) -> String {
    let mut out = Vec::new();
    db.export(&mut out).unwrap();
    String::from_utf8(out).unwrap()
}

fn import(dir: &Path, name: &str, text: &str) -> trunkdb::Result<Summary> {
    open(dir, name).import(text.as_bytes())
}

/// The line an import failed at, and its message.
fn failed_at(result: trunkdb::Result<Summary>) -> (usize, String) {
    match result {
        Err(Error::Import { line, message, .. }) => (line, message),
        other => panic!("{other:?}"),
    }
}

/// §165.1: the end line, last, with the counts; for an empty database,
/// and from a snapshot as from the database.
#[test]
fn an_export_ends_with_what_it_holds() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "db.trunkdb");
    assert_eq!(
        export(&db),
        "{\"$trunkdb_export\":4}\n{\"$end\":{\"collections\":0,\"documents\":0}}\n"
    );
    sample(&db);
    let text = export(&db);
    assert!(text.starts_with("{\"$trunkdb_export\":4}\n"), "{text}");
    assert!(
        text.ends_with("\n{\"$end\":{\"collections\":3,\"documents\":8}}\n"),
        "{text}"
    );
    let mut snapshot = Vec::new();
    let summary = db.snapshot().unwrap().export(&mut snapshot).unwrap();
    assert_eq!(String::from_utf8(snapshot).unwrap(), text);
    assert!(summary.ended);
    assert_eq!((summary.collections, summary.documents), (3, 8));

    let imported = import(dir.path(), "copy.trunkdb", &text).unwrap();
    assert!(imported.ended);
    assert_eq!((imported.collections, imported.documents), (3, 8));
}

/// §165.1: an export cut after any line fails to import, at the line
/// after the last, and so does every other way its end goes wrong.
#[test]
fn an_import_finds_an_export_cut_short() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "db.trunkdb");
    sample(&db);
    let text = export(&db);
    let lines: Vec<&str> = text.lines().collect();
    for kept in 1..lines.len() {
        let cut = lines[..kept].join("\n") + "\n";
        let (line, message) = failed_at(import(dir.path(), &format!("cut{kept}.trunkdb"), &cut));
        assert_eq!(line, kept + 1, "{cut}");
        assert!(message.contains("cut short"), "{message}");
        // Without its last newline too.
        let cut = lines[..kept].join("\n");
        let (line, _) = failed_at(import(dir.path(), &format!("bare{kept}.trunkdb"), &cut));
        assert_eq!(line, kept + 1, "{cut}");
    }
    // Empty lines after the end line are fine.
    let padded = format!("{text}\n\n");
    assert!(import(dir.path(), "padded.trunkdb", &padded).unwrap().ended);

    let end = lines.len();
    for (i, wrong) in [
        "{\"$end\":{\"collections\":2,\"documents\":8}}",
        "{\"$end\":{\"collections\":3,\"documents\":9}}",
        "{\"$end\":{\"collections\":3,\"documents\":7}}",
    ]
    .into_iter()
    .enumerate()
    {
        let changed = lines[..end - 1].join("\n") + "\n" + wrong + "\n";
        let (line, message) = failed_at(import(dir.path(), &format!("wrong{i}.trunkdb"), &changed));
        assert_eq!(line, end, "{wrong}");
        assert!(message.contains("came before it"), "{message}");
    }
    for (i, malformed) in [
        "{\"$end\":{\"collections\":3}}",
        "{\"$end\":{\"collections\":3,\"documents\":8,\"more\":1}}",
        "{\"$end\":8}",
        "{\"$end\":{\"collections\":-3,\"documents\":8}}",
    ]
    .into_iter()
    .enumerate()
    {
        let changed = lines[..end - 1].join("\n") + "\n" + malformed + "\n";
        let (line, message) = failed_at(import(
            dir.path(),
            &format!("malformed{i}.trunkdb"),
            &changed,
        ));
        assert_eq!(line, end, "{malformed}");
        assert!(message.contains("an end line must be"), "{message}");
    }
    let after = format!("{text}{{\"name\":\"Dee\"}}\n");
    let (line, message) = failed_at(import(dir.path(), "after.trunkdb", &after));
    assert_eq!(
        (line, message.as_str()),
        (end + 1, "a line after the end line")
    );
}

/// §165.1: versions 1 and 2 import as before, without an end line, and
/// `ended` says so; an end line in one is a mistake.
#[test]
fn older_exports_import_without_an_end_line() {
    let dir = tempfile::tempdir().unwrap();
    for version in [1, 2] {
        let text =
            format!("{{\"$trunkdb_export\":{version}}}\n{{\"$collection\":\"c\"}}\n{{\"a\":1}}\n");
        let summary = import(dir.path(), &format!("v{version}.trunkdb"), &text).unwrap();
        assert_eq!((summary.documents, summary.ended), (1, false));
        let ended = format!("{text}{{\"$end\":{{\"collections\":1,\"documents\":1}}}}\n");
        let (line, message) = failed_at(import(dir.path(), &format!("e{version}.trunkdb"), &ended));
        assert_eq!(line, 4);
        assert!(message.contains(&format!("version {version}")), "{message}");
    }
    // A document with more than `$end` in it is a document, as before.
    let text = "{\"$trunkdb_export\":3}\n{\"$collection\":\"c\"}\n{\"$end\":1,\"b\":2}\n\
                {\"$end\":{\"collections\":1,\"documents\":1}}\n";
    let summary = import(dir.path(), "doc.trunkdb", text).unwrap();
    assert_eq!((summary.documents, summary.ended), (1, true));
}

/// §165.2: a restore tried and checked, and every file it made gone,
/// whatever came of it.
#[test]
fn verify_export_restores_and_counts() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "db.trunkdb");
    sample(&db);
    let text = export(&db);
    let at = dir.path().join("verify.trunkdb");
    let wal = dir.path().join("verify.trunkdb.wal");
    let gone = || assert!(!at.exists() && !wal.exists(), "left behind");

    let verified = verify_export(text.as_bytes(), &at).unwrap();
    assert!(verified.is_ok() && verified.ended, "{verified:?}");
    assert_eq!(
        (verified.collections, verified.documents, verified.indexes),
        (3, 8, 2)
    );
    gone();

    // Cut short; a line damaged; an id twice; a unique index broken.
    let lines: Vec<&str> = text.lines().collect();
    let cut = lines[..4].join("\n") + "\n";
    let damaged = text.replacen("\"name\":\"Bob\"", "\"name\":\"Bob", 1);
    let twice = {
        let mut lines = lines.clone();
        let copy = lines[3];
        lines.insert(4, copy);
        lines.join("\n") + "\n"
    };
    let broken = text.replacen("\"name\":\"Bob\"", "\"name\":\"Ada\"", 1);
    for (bad, says) in [
        (cut.as_str(), "cut short"),
        (damaged.as_str(), "not JSON"),
        (twice.as_str(), ""),
        (broken.as_str(), ""),
    ] {
        match verify_export(bad.as_bytes(), &at) {
            Err(Error::Import { message, .. }) => assert!(message.contains(says), "{message}"),
            Err(Error::DuplicateId { .. } | Error::DuplicateValue { .. }) if says.is_empty() => {}
            other => panic!("{bad}: {other:?}"),
        }
        gone();
    }

    // An older export restores, but can't say it is whole.
    let older = "{\"$trunkdb_export\":2}\n{\"$collection\":\"c\"}\n{\"a\":1}\n";
    let verified = verify_export(older.as_bytes(), &at).unwrap();
    assert!(verified.is_ok() && !verified.ended);
    gone();

    // An `at` that exists is refused and left as it was.
    std::fs::write(&at, b"mine").unwrap();
    let refused = verify_export(text.as_bytes(), &at);
    assert!(
        matches!(refused, Err(Error::Io(ref e)) if e.kind() == std::io::ErrorKind::AlreadyExists),
        "{refused:?}"
    );
    assert_eq!(std::fs::read(&at).unwrap(), b"mine");
    std::fs::remove_file(&at).unwrap();
    std::fs::write(&wal, b"mine").unwrap();
    assert!(verify_export(text.as_bytes(), &at).is_err());
    assert_eq!(std::fs::read(&wal).unwrap(), b"mine");
    assert!(!at.exists());
}
