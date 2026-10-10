//! The `trunkdb` command, run as a real process (SPEC §39).

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn trunkdb(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_trunkdb"))
        .args(args)
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

const EXPORT: &str = r#"{"$trunkdb_export":1}
{"$collection":"users","$indexes":["age",{"field":"email","unique":true},{"field":"nick","sparse":true},{"fields":["team","email"],"unique":true,"sparse":true}]}
{"name":"Ada","age":36,"email":"a@x"}
{"name":"Bob","age":41,"email":"b@x"}
{"$collection":"empty"}
"#;

fn path(dir: &Path, name: &str) -> String {
    dir.join(name).to_str().unwrap().to_string()
}

#[test]
fn usage_and_help() {
    let output = trunkdb(&[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("usage: trunkdb"));
    let output = trunkdb(&["info"]);
    assert_eq!(output.status.code(), Some(2));
    let output = trunkdb(&["--help"]);
    assert!(output.status.success());
    assert!(stdout(&output).contains("commands:"));
}

#[test]
fn a_missing_file_is_an_error_and_isnt_created() {
    let dir = tempfile::tempdir().unwrap();
    let missing = path(dir.path(), "typo.trunkdb");
    for command in ["info", "check", "export"] {
        let output = trunkdb(&[command, &missing]);
        assert_eq!(output.status.code(), Some(1), "{command}");
        assert!(stderr(&output).contains("no such file"), "{command}");
    }
    assert!(!Path::new(&missing).exists());
}

#[test]
fn import_info_check_and_export_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let (db, input, out) = (
        path(dir.path(), "db.trunkdb"),
        path(dir.path(), "in.jsonl"),
        path(dir.path(), "out.jsonl"),
    );
    std::fs::write(&input, EXPORT).unwrap();

    let output = trunkdb(&["import", &db, &input]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("imported 2 collections, 2 documents"));

    let output = trunkdb(&["info", &db]);
    assert!(output.status.success());
    let info = stdout(&output);
    assert!(info.contains("format 11"), "{info}");
    assert!(
        info.contains(
            "users: 2 documents, indexes: age, email (unique), nick (sparse), \
             (team, email) (unique, sparse)"
        ),
        "{info}"
    );
    assert!(info.contains("empty: 0 documents"), "{info}");

    let output = trunkdb(&["check", &db]);
    assert!(output.status.success());
    assert_eq!(stdout(&output), "ok: 2 collections, 2 documents, 9 pages\n");

    // To a file, and to standard output — the same, and it imports back.
    let output = trunkdb(&["export", &db, &out]);
    assert!(output.status.success());
    let exported = std::fs::read_to_string(&out).unwrap();
    let output = trunkdb(&["export", &db]);
    assert_eq!(stdout(&output), exported);
    assert!(
        exported.starts_with("{\"$trunkdb_export\":2}\n"),
        "{exported}"
    );
    assert!(exported.contains(r#"{"field":"email","unique":true}"#));
    assert!(exported.contains(r#"{"field":"nick","sparse":true}"#));

    // `-` reads standard input.
    let copy = path(dir.path(), "copy.trunkdb");
    let mut child = Command::new(env!("CARGO_BIN_EXE_trunkdb"))
        .args(["import", &copy, "-"])
        .stdin(Stdio::piped())
        .output_with(|stdin| stdin.write_all(exported.as_bytes()));
    assert!(child.status.success(), "{}", stderr(&child));
    child = trunkdb(&["export", &copy]);
    assert_eq!(stdout(&child), exported);
}

#[test]
fn check_reports_a_damaged_page_and_fails() {
    let dir = tempfile::tempdir().unwrap();
    let (db, input) = (path(dir.path(), "db.trunkdb"), path(dir.path(), "in.jsonl"));
    std::fs::write(&input, EXPORT).unwrap();
    assert!(trunkdb(&["import", &db, &input]).status.success());

    // One changed byte in the last page, as a disk error would leave it.
    let mut bytes = std::fs::read(&db).unwrap();
    let page = bytes.len() / 8192 - 1;
    bytes[page * 8192 + 100] ^= 0xFF;
    std::fs::write(&db, bytes).unwrap();

    let output = trunkdb(&["check", &db]);
    assert_eq!(output.status.code(), Some(1));
    let report = stdout(&output);
    assert!(
        report.contains(&format!("problem: page {page} is damaged")),
        "{report}"
    );
}

#[test]
fn compact_shrinks_a_file_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = path(dir.path(), "db.trunkdb");
    {
        let database = trunkdb::Database::open(&db).unwrap();
        let docs = database.collection::<trunkdb::Document>("docs");
        for i in 0..300 {
            docs.insert(trunkdb::Document::String(format!("{i:0>2000}")))
                .unwrap();
        }
        docs.delete_many(trunkdb::query::Filter::new()).unwrap();
    }
    let before = std::fs::metadata(&db).unwrap().len();

    let output = trunkdb(&["compact", &db]);
    assert!(output.status.success(), "{}", stderr(&output));
    let report = stdout(&output);
    assert!(report.starts_with("compacted: "), "{report}");
    assert!(report.contains(" smaller"), "{report}");
    let after = std::fs::metadata(&db).unwrap().len();
    assert!(after * 10 < before, "{before} -> {after}");

    let output = trunkdb(&["compact", &db]);
    assert!(output.status.success());
    assert!(stdout(&output).starts_with("already compact: "));
    assert_eq!(std::fs::metadata(&db).unwrap().len(), after);
    assert!(trunkdb(&["check", &db]).status.success());
}

#[test]
fn a_file_in_use_is_named_as_such() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = path(dir.path(), "db.trunkdb");
    let db = trunkdb::Database::open(&db_path).unwrap();
    let output = trunkdb(&["info", &db_path]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("in use by another program"),
        "{}",
        stderr(&output)
    );
    drop(db);
    assert!(trunkdb(&["info", &db_path]).status.success());
}

/// `info`, `check` and `export` open the file read-only (SPEC §88), so
/// they go beside a read-only open of it, and `compact` doesn't.
#[test]
fn looking_into_a_file_opens_it_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let (db_path, out) = (
        path(dir.path(), "db.trunkdb"),
        path(dir.path(), "out.jsonl"),
    );
    let db = trunkdb::Database::open(&db_path).unwrap();
    db.collection::<trunkdb::Document>("users")
        .insert(trunkdb::Document::Int(1))
        .unwrap();
    drop(db);
    let read_only = trunkdb::OpenOptions::default().read_only(true);
    let db = trunkdb::Database::open_with(&db_path, read_only).unwrap();

    let output = trunkdb(&["info", &db_path]);
    assert!(
        stdout(&output).contains("users: 1 document"),
        "{}",
        stderr(&output)
    );
    assert!(trunkdb(&["check", &db_path]).status.success());
    assert!(trunkdb(&["export", &db_path, &out]).status.success());
    assert!(std::fs::read_to_string(&out).unwrap().contains("users"));
    let output = trunkdb(&["compact", &db_path]);
    assert!(stderr(&output).contains("in use by another program"));
    drop(db);
    assert!(trunkdb(&["compact", &db_path]).status.success());
}

/// `Command::output`, with something written to the child's stdin first.
trait OutputWith {
    fn output_with(
        &mut self,
        write: impl FnOnce(&mut std::process::ChildStdin) -> std::io::Result<()>,
    ) -> Output;
}

impl OutputWith for Command {
    fn output_with(
        &mut self,
        write: impl FnOnce(&mut std::process::ChildStdin) -> std::io::Result<()>,
    ) -> Output {
        let mut child = self
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        write(&mut stdin).unwrap();
        drop(stdin); // end of input
        child.wait_with_output().unwrap()
    }
}

#[test]
fn export_refuses_the_database_itself_and_replaces_the_output_whole() {
    let dir = tempfile::tempdir().unwrap();
    let (db, input, alias, out) = (
        path(dir.path(), "db.trunkdb"),
        path(dir.path(), "in.jsonl"),
        path(dir.path(), "alias.trunkdb"),
        path(dir.path(), "out.jsonl"),
    );
    std::fs::write(&input, EXPORT).unwrap();
    assert!(trunkdb(&["import", &db, &input]).status.success());
    let before = std::fs::read(&db).unwrap();

    // The database's own path, and a hard link to it. Only where files
    // have inodes is the link refused (SPEC §84.1); elsewhere the rename
    // replaces the link's name and the database is left alone all the same.
    std::fs::hard_link(&db, &alias).unwrap();
    for (target, refused) in [(&db, true), (&alias, cfg!(unix))] {
        let output = trunkdb(&["export", &db, target]);
        if refused {
            assert_eq!(output.status.code(), Some(1));
            assert!(
                stderr(&output).contains("choose another file"),
                "{}",
                stderr(&output)
            );
        }
        assert_eq!(std::fs::read(&db).unwrap(), before);
    }
    assert!(trunkdb(&["check", &db]).status.success());

    // No temporary file is left behind, and an existing output is replaced whole.
    std::fs::write(&out, "old").unwrap();
    assert!(trunkdb(&["export", &db, &out]).status.success());
    assert!(
        std::fs::read_to_string(&out)
            .unwrap()
            .starts_with("{\"$trunkdb_export\"")
    );
    let leftovers = std::fs::read_dir(dir.path())
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp")
        })
        .count();
    assert_eq!(leftovers, 0);

    // A failed export (unwritable directory) leaves nothing and touches nothing.
    let missing = path(&dir.path().join("nope"), "x.jsonl");
    assert_eq!(trunkdb(&["export", &db, &missing]).status.code(), Some(1));
}

#[test]
fn import_stops_at_a_line_over_the_limit_unless_it_is_turned_off() {
    let dir = tempfile::tempdir().unwrap();
    let (db, input) = (path(dir.path(), "db.trunkdb"), path(dir.path(), "in.jsonl"));
    std::fs::write(&input, EXPORT).unwrap();

    let output = trunkdb(&["import", &db, &input, "--max-line", "60"]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(
        message.contains("line 2") && message.contains("longer than 60"),
        "{message}"
    );

    let output = trunkdb(&["import", &db, &input, "--max-line", "x"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("--max-line"));

    let big = path(dir.path(), "big.trunkdb");
    for limit in ["none", "1000"] {
        let output = trunkdb(&["import", &big, &input, "--max-line", limit]);
        assert!(output.status.success(), "{}", stderr(&output));
        std::fs::remove_file(&big).unwrap();
        let _ = std::fs::remove_file(format!("{big}.wal"));
    }
}
