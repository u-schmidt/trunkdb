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
{"$collection":"users","$indexes":["age",{"field":"email","unique":true}]}
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
    assert!(info.contains("format 5"), "{info}");
    assert!(
        info.contains("users: 2 documents, indexes: age, email (unique)"),
        "{info}"
    );
    assert!(info.contains("empty: 0 documents"), "{info}");

    let output = trunkdb(&["check", &db]);
    assert!(output.status.success());
    assert_eq!(stdout(&output), "ok: 2 collections, 2 documents, 7 pages\n");

    // To a file, and to standard output — the same, and it imports back.
    let output = trunkdb(&["export", &db, &out]);
    assert!(output.status.success());
    let exported = std::fs::read_to_string(&out).unwrap();
    let output = trunkdb(&["export", &db]);
    assert_eq!(stdout(&output), exported);
    assert!(exported.contains(r#"{"field":"email","unique":true}"#));

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
fn check_reports_a_leaked_page_and_fails() {
    let dir = tempfile::tempdir().unwrap();
    let (db, input) = (path(dir.path(), "db.trunkdb"), path(dir.path(), "in.jsonl"));
    std::fs::write(&input, EXPORT).unwrap();
    assert!(trunkdb(&["import", &db, &input]).status.success());

    // One more page, counted in the header, owned by nothing.
    let mut bytes = std::fs::read(&db).unwrap();
    let pages = u64::from_le_bytes(bytes[12..20].try_into().unwrap());
    bytes[12..20].copy_from_slice(&(pages + 1).to_le_bytes());
    bytes.extend_from_slice(&[0u8; 8192]);
    std::fs::write(&db, bytes).unwrap();

    let output = trunkdb(&["check", &db]);
    assert_eq!(output.status.code(), Some(1));
    let report = stdout(&output);
    assert!(
        report.contains(&format!("problem: page {pages} belongs to nothing")),
        "{report}"
    );
    assert!(report.contains("1 problem: "), "{report}");
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
