//! The `trunkdb` command, run as a real process (SPEC §39).

use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Mutex, PoisonError};

/// Held while a process starts, and while a test closes a database
/// whose file is opened again. On Linux a child holds a copy of every
/// file descriptor of this process from its fork to its exec, and a
/// file lock (`flock`) lasts while any copy of its file is open: a
/// database closed in that window stays locked until the child's exec,
/// and opening it again fails with `WouldBlock`. The tests run on
/// threads of one process, so another test's spawn can fall into this
/// one's close. `spawn` returns only after the exec, so a close that
/// holds this lock releases the file lock when it returns.
static SPAWNS: Mutex<()> = Mutex::new(());

/// `command` started, its exec done (`SPAWNS`).
fn spawn(command: &mut Command) -> Child {
    let _no_close = SPAWNS.lock().unwrap_or_else(PoisonError::into_inner);
    command.spawn().unwrap()
}

/// `db` closed with no child between its fork and its exec, so its file
/// is free when this returns (`SPAWNS`).
fn close(db: trunkdb::Database) {
    let _no_spawn = SPAWNS.lock().unwrap_or_else(PoisonError::into_inner);
    drop(db);
}

/// A child started between its fork and its exec holds a copy of a
/// database's file and its lock; `close` waits for its exec, so the
/// file opens again right after. Forced here with a child that waits
/// before its exec (`pre_exec` takes the fork path on every Unix).
#[cfg(unix)]
#[test]
fn a_database_closed_while_a_process_starts_opens_again() {
    use std::os::unix::process::CommandExt;
    let dir = tempfile::tempdir().unwrap();
    let db_path = path(dir.path(), "db.trunkdb");
    let db = trunkdb::Database::open(&db_path).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_trunkdb"));
    command.arg("--help").stdout(Stdio::null());
    // SAFETY: sleeps and returns, which is all the child does before
    // its exec.
    unsafe {
        command.pre_exec(|| {
            std::thread::sleep(std::time::Duration::from_millis(300));
            Ok(())
        });
    }
    let started = std::thread::spawn(move || spawn(&mut command).wait().unwrap());
    std::thread::sleep(std::time::Duration::from_millis(100));
    close(db);
    let read_only = trunkdb::OpenOptions::default().read_only(true);
    trunkdb::Database::open_with(&db_path, read_only).unwrap();
    assert!(started.join().unwrap().success());
}

fn trunkdb(args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_trunkdb"));
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    spawn(&mut command).wait_with_output().unwrap()
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

/// A file this build can't read is reported in the same words as
/// before the error was typed (SPEC §92), and nothing is changed.
#[test]
fn a_file_it_cannot_read_is_reported_as_before() {
    let dir = tempfile::tempdir().unwrap();
    let (notes, newer, input) = (
        path(dir.path(), "notes.txt"),
        path(dir.path(), "newer.trunkdb"),
        path(dir.path(), "in.jsonl"),
    );
    std::fs::write(&notes, b"my notes").unwrap();
    std::fs::write(&input, EXPORT).unwrap();
    assert!(trunkdb(&["import", &newer, &input]).status.success());
    let mut bytes = std::fs::read(&newer).unwrap();
    bytes[28..32].copy_from_slice(&18u32.to_le_bytes());
    std::fs::write(&newer, &bytes).unwrap();

    for (file, message) in [
        (&notes, "not a trunkdb file (bad magic)".to_string()),
        (
            &newer,
            "file has format 18, newer than this build's 17 — open it with a newer trunkdb"
                .to_string(),
        ),
    ] {
        let before = std::fs::read(file).unwrap();
        for command in ["info", "check", "compact"] {
            let output = trunkdb(&[command, file]);
            assert_eq!(output.status.code(), Some(1), "{command}");
            assert_eq!(
                stderr(&output),
                format!("trunkdb: {file}: {message}\n"),
                "{command}"
            );
        }
        assert_eq!(std::fs::read(file).unwrap(), before);
    }
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
    assert!(info.contains("format 17"), "{info}");
    // The releases (SPEC §125), beside the format.
    let release = trunkdb::RELEASE;
    assert!(
        info.contains(&format!("\n  written by {release}, created by {release}\n")),
        "{info}"
    );
    assert!(
        info.contains(
            "users: 2 documents, indexes: age, email (unique), nick (sparse), \
             (team, email) (unique, sparse)"
        ),
        "{info}"
    );
    assert!(info.contains("empty: 0 documents"), "{info}");
    // The sizes (SPEC §93), each under the lines that were there before.
    assert!(info.contains("\n  pages: 1 header, 1 catalog, "), "{info}");
    assert!(info.contains("\n  compact would "), "{info}");
    assert!(
        info.contains(
            "\n    0 bytes of documents in 0 data pages, 0 bytes free in them; indexes 1 page\n"
        ),
        "{info}"
    );
    assert!(info.contains(" of documents in 1 data page, "), "{info}");

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
        exported.starts_with("{\"$trunkdb_export\":4}\n"),
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
    // As JSON (SPEC §155): the same problems, and the same exit code.
    let output = trunkdb(&["check", &db, "--json"]);
    assert_eq!(output.status.code(), Some(1));
    let json: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(json["ok"], false);
    let problems = json["problems"].as_array().unwrap();
    assert!(
        problems.iter().any(|p| p
            .as_str()
            .unwrap()
            .contains(&format!("page {page} is damaged"))),
        "{json}"
    );
}

#[test]
fn compact_shrinks_a_file_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = path(dir.path(), "db.trunkdb");
    let database = trunkdb::Database::open(&db).unwrap();
    let docs = database.collection::<trunkdb::Document>("docs");
    for i in 0..300 {
        let text = trunkdb::Document::String(format!("{i:0>2000}"));
        docs.insert(object("text", text)).unwrap();
    }
    docs.delete_many(trunkdb::query::Filter::new()).unwrap();
    drop(docs);
    close(database);
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

/// A file another program writes: a write is refused, the file named as
/// in use; looking into it goes beside that program (SPEC §185).
#[test]
fn a_file_in_use_is_named_as_such() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = path(dir.path(), "db.trunkdb");
    let db = trunkdb::Database::open(&db_path).unwrap();
    let output = trunkdb(&["insert", &db_path, "users", "{}"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("in use by another program"),
        "{}",
        stderr(&output)
    );
    assert!(trunkdb(&["info", &db_path]).status.success());
    close(db);
    let output = trunkdb(&["insert", &db_path, "users", "{}", "--create"]);
    assert!(output.status.success(), "{}", stderr(&output));
}

/// `info`, `check` and `export` open the file read-only (SPEC §88), so
/// they go beside a read-only open of it; and `compact` goes between its
/// reads, as a reader beside a writer holds nothing between its calls
/// (SPEC §185.4).
#[test]
fn looking_into_a_file_opens_it_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let (db_path, out) = (
        path(dir.path(), "db.trunkdb"),
        path(dir.path(), "out.jsonl"),
    );
    let db = trunkdb::Database::open(&db_path).unwrap();
    db.collection::<trunkdb::Document>("users")
        .insert(object("n", trunkdb::Document::Int(1)))
        .unwrap();
    close(db);
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
    let output = trunkdb(&["find", &db_path, "users", "n = 1"]);
    assert_eq!(stderr(&output), "found 1 document\n");
    let output = trunkdb(&["fields", &db_path, "users"]);
    assert_eq!(stderr(&output), "2 paths in 1 document\n");
    // And what came with SPEC §155.
    assert_eq!(stdout(&trunkdb(&["count", &db_path, "users"])), "1\n");
    assert!(trunkdb(&["info", &db_path, "--json"]).status.success());
    assert!(trunkdb(&["check", &db_path, "--json"]).status.success());
    assert!(
        trunkdb(&["fields", &db_path, "users", "--json"])
            .status
            .success()
    );
    let found = stdout(&trunkdb(&["find", &db_path, "users"]));
    let id = id_in(&found);
    assert!(trunkdb(&["get", &db_path, "users", &id]).status.success());
    let output = trunkdb(&["compact", &db_path]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        db.collection::<trunkdb::Document>("users")
            .count(trunkdb::query::Filter::new())
            .unwrap(),
        1
    );
    close(db);
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
        let mut child = spawn(self.stdout(Stdio::piped()).stderr(Stdio::piped()));
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

    // Through a symlink to the database, its WAL is the file's, beside
    // it, and that is refused too (SPEC §127).
    #[cfg(unix)]
    {
        let link = path(dir.path(), "link.trunkdb");
        let wal = format!("{db}.wal");
        std::os::unix::fs::symlink(&db, &link).unwrap();
        let wal_before = std::fs::read(&wal).unwrap();
        let output = trunkdb(&["export", &link, &wal]);
        assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
        assert!(stderr(&output).contains("choose another file"));
        assert_eq!(std::fs::read(&wal).unwrap(), wal_before);
        std::fs::remove_file(&link).unwrap();
    }

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

/// `find` (SPEC §112): what a filter finds, one JSON line each, which
/// reads back as the stored document; the count to stderr.
#[test]
fn find_prints_what_a_filter_finds_as_json_lines() {
    let dir = tempfile::tempdir().unwrap();
    let (db, input) = (path(dir.path(), "db.trunkdb"), path(dir.path(), "in.jsonl"));
    std::fs::write(&input, EXPORT).unwrap();
    assert!(trunkdb(&["import", &db, &input]).status.success());

    let lines = |output: &Output| -> Vec<trunkdb::Document> {
        assert!(output.status.success(), "{}", stderr(output));
        stdout(output)
            .lines()
            .map(|line| line.parse().unwrap())
            .collect()
    };
    let output = trunkdb(&["find", &db, "users"]);
    let all = lines(&output);
    assert_eq!(stderr(&output), "found 2 documents\n");
    let opened = trunkdb::Database::open_with(&db, trunkdb::OpenOptions::default().read_only(true));
    let users = opened
        .as_ref()
        .unwrap()
        .collection::<trunkdb::Document>("users");
    for doc in &all {
        let trunkdb::Document::Object(fields) = doc else {
            panic!("{doc}")
        };
        let Some(trunkdb::Document::Id(id)) = fields.get("_id") else {
            panic!("{doc}")
        };
        assert_eq!(users.get(id).unwrap().as_ref(), Some(doc));
    }
    close(opened.unwrap());

    let output = trunkdb(&["find", &db, "users", "age > 40"]);
    assert_eq!(lines(&output).len(), 1);
    assert!(
        stdout(&output).contains(r#""name":"Bob""#),
        "{}",
        stdout(&output)
    );
    assert_eq!(stderr(&output), "found 1 document\n");

    // Words after the collection are one filter.
    let output = trunkdb(&[
        "find", &db, "users", "select", "name", "order", "by", "age", "desc",
    ]);
    let names: Vec<String> = lines(&output).iter().map(|doc| doc.to_string()).collect();
    assert_eq!(names.len(), 2);
    assert!(names[0].ends_with(r#","name":"Bob"}"#), "{names:?}");
    assert!(names[1].ends_with(r#","name":"Ada"}"#), "{names:?}");

    let output = trunkdb(&["find", &db, "empty"]);
    assert_eq!(
        (stdout(&output), stderr(&output)),
        (String::new(), "found 0 documents\n".to_string())
    );
}

#[test]
fn find_names_a_mistake_and_where_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let (db, input) = (path(dir.path(), "db.trunkdb"), path(dir.path(), "in.jsonl"));
    std::fs::write(&input, EXPORT).unwrap();
    assert!(trunkdb(&["import", &db, &input]).status.success());

    let output = trunkdb(&[
        "find",
        &db,
        "users",
        "age >= 30 and\n  name = Ada\n  order by name",
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stdout(&output), "");
    assert_eq!(
        stderr(&output),
        "trunkdb: filter: expected a value, found `Ada` at line 2 column 10\n    name = Ada\n           ^\n"
    );

    let output = trunkdb(&["find", &db, "user"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "trunkdb: user: no such collection\n");

    let missing = path(dir.path(), "typo.trunkdb");
    let output = trunkdb(&["find", &missing, "users"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("no such file"));
    assert!(!Path::new(&missing).exists());

    assert_eq!(trunkdb(&["find", &db]).status.code(), Some(2));
}

/// `find … --explain` (SPEC §113): how the query ran, a line each, in
/// place of the documents.
#[test]
fn find_explain_says_how_it_found_them() {
    let dir = tempfile::tempdir().unwrap();
    let (db, input) = (path(dir.path(), "db.trunkdb"), path(dir.path(), "in.jsonl"));
    std::fs::write(&input, EXPORT).unwrap();
    assert!(trunkdb(&["import", &db, &input]).status.success());

    // All but the time, which differs from run to run.
    let explained = |args: &[&str]| -> String {
        let output = trunkdb(args);
        assert!(output.status.success(), "{}", stderr(&output));
        assert_eq!(stderr(&output), "");
        let text = stdout(&output);
        let (rest, time) = text.trim_end().rsplit_once('\n').unwrap();
        assert!(
            time.starts_with("time: ") && time.ends_with(" ms"),
            "{text}"
        );
        format!("{rest}\n")
    };
    assert_eq!(
        explained(&["find", &db, "users", "age > 40", "--explain"]),
        "plan: index on age\n\
         read: age, where age > 40, 1 entry, its entries decide the filter\n\
         instead: scan of every document, 2 entries\n\
         keys examined: 1\n\
         documents examined: 1\n\
         documents matched: 1\n\
         documents returned: 1\n\
         answered from index entries: 0\n"
    );
    assert_eq!(
        explained(&["find", &db, "users", "--explain"]),
        "plan: scan of every document\n\
         read: _id, the whole index, 2 entries, its entries decide the filter\n\
         keys examined: 2\n\
         documents examined: 2\n\
         documents matched: 2\n\
         documents returned: 2\n\
         answered from index entries: 0\n"
    );
    let text = explained(&[
        "find",
        &db,
        "users",
        "select",
        "email",
        "where",
        "email",
        "=",
        "\"b@x\"",
        "or",
        "age",
        "<",
        "40",
        "order",
        "by",
        "age",
        "desc",
        "limit",
        "1",
        "--explain",
    ]);
    assert_eq!(
        text,
        "plan: indexes on email, age, together\n\
         read: email, where email = \"b@x\", 1 entry\n\
         read: age, where age < 40, 1 entry\n\
         sort: 0 keys from the index order, 1 in memory\n\
         instead: index on age, in its order, 2 entries\n\
         instead: scan of every document, 2 entries\n\
         keys examined: 2\n\
         documents examined: 2\n\
         documents matched: 2\n\
         documents returned: 1\n\
         answered from index entries: 0\n"
    );
    let text = explained(&[
        "find",
        &db,
        "users",
        "order by age desc limit 1",
        "--explain",
    ]);
    assert!(
        text.starts_with("plan: index on age, in its order\n"),
        "{text}"
    );
    assert!(text.contains(", backward"), "{text}");
    assert!(
        text.contains("sort: 1 key from the index order, 0 in memory\n"),
        "{text}"
    );

    let text = explained(&[
        "find",
        &db,
        "users",
        r#"select email where email = "a@x""#,
        "--explain",
    ]);
    assert_eq!(
        text,
        "plan: index on email\n\
         read: email, where email = \"a@x\", 1 entry, its entries decide the filter\n\
         instead: scan of every document, 2 entries\n\
         keys examined: 1\n\
         documents examined: 0\n\
         documents matched: 1\n\
         documents returned: 1\n\
         answered from index entries: 1\n"
    );

    // A hint (SPEC §132): the plan it asked for, said so.
    let text = explained(&["find", &db, "users", "age > 40 use no index", "--explain"]);
    assert!(
        text.starts_with("plan: scan of every document, as hinted\n"),
        "{text}"
    );
    // The planner's own plan among the ones passed over (SPEC §133).
    assert!(
        text.contains("instead: index on age, 1 entry, its entries decide the filter, fewer\n"),
        "{text}"
    );
    let output = trunkdb(&["find", &db, "users", "age > 40 use index name"]);
    assert_eq!(
        (output.status.code(), stdout(&output)),
        (Some(1), String::new())
    );
    assert!(
        stderr(&output).contains("no such index"),
        "{}",
        stderr(&output)
    );

    // Its mistakes are `find`'s.
    let output = trunkdb(&["find", &db, "users", "age >", "--explain"]);
    assert_eq!(
        (output.status.code(), stdout(&output)),
        (Some(2), String::new())
    );
    let output = trunkdb(&["find", &db, "user", "--explain"]);
    assert_eq!(stderr(&output), "trunkdb: user: no such collection\n");
}

/// `find … | head`: the reader gone, `find` stops, and that's no error.
#[test]
fn find_stops_quietly_when_its_reader_goes() {
    use std::io::Read;
    let dir = tempfile::tempdir().unwrap();
    let db_path = path(dir.path(), "db.trunkdb");
    let db = trunkdb::Database::open(&db_path).unwrap();
    let docs = db.collection::<trunkdb::Document>("docs");
    let mut batch = db.batch();
    for n in 0..20_000 {
        batch
            .insert(&docs, object("n", trunkdb::Document::Int(n)))
            .unwrap();
    }
    batch.commit().unwrap();
    drop(docs); // it holds the database open too
    close(db);

    let mut child = spawn(
        Command::new(env!("CARGO_BIN_EXE_trunkdb"))
            .args(["find", &db_path, "docs"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    );
    let mut first = [0u8; 100];
    let read = child.stdout.take().unwrap().read_exact(&mut first);
    let output = child.wait_with_output().unwrap();
    assert!(read.is_ok(), "{read:?}: {}", stderr(&output));
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stderr(&output), "");
}

/// `info … | head`: the reader gone, `info` stops, and that's no error,
/// as for `find`. Enough collections that their lines are more than a
/// pipe holds, so the write after the reader goes fails.
#[test]
fn info_stops_quietly_when_its_reader_goes() {
    use std::io::Read;
    let dir = tempfile::tempdir().unwrap();
    let db_path = path(dir.path(), "db.trunkdb");
    let db = trunkdb::Database::open(&db_path).unwrap();
    let mut batch = db.batch();
    for n in 0..3_000 {
        let collection = db.collection::<trunkdb::Document>(&format!("collection{n:05}"));
        batch
            .insert(&collection, object("n", trunkdb::Document::Int(n)))
            .unwrap();
    }
    batch.commit().unwrap();
    close(db);

    let mut child = spawn(
        Command::new(env!("CARGO_BIN_EXE_trunkdb"))
            .args(["info", &db_path])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    );
    let mut first = [0u8; 100];
    let read = child.stdout.take().unwrap().read_exact(&mut first);
    let output = child.wait_with_output().unwrap();
    assert!(read.is_ok(), "{read:?}: {}", stderr(&output));
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stderr(&output), "");
}

/// Documents of every shape `fields` sums up (SPEC §121): kinds that
/// differ from one document to the next, objects and arrays at any depth,
/// names a filter writes in backticks, and one no path can name; a name
/// that sorts before `_id`, and one of more bytes than letters.
const SHAPES: &str = r#"{"$trunkdb_export":2}
{"$collection":"users","$indexes":["age"]}
{"name":"Ada","age":36,"address":{"city":"Berlin"},"tags":["a","b"],"first name":"A","grid":[[1,2],[3]],"Zone":1,"größe":2}
{"name":"Bob","age":41.5,"address":"unknown","tags":[],"order":1,"a`b":true,"grid":{"n":1}}
{"name":"Cy","age":null,"address":{"city":"Paris"},"tags":["x",{"k":1}],"a.b":{"c":2},"seen":{"$date":"2026-01-01T00:00:00Z"},"bin":{"$binary":"AAE="}}
{"$collection":"empty"}
"#;

#[test]
fn fields_sums_up_every_path_the_documents_have() {
    let dir = tempfile::tempdir().unwrap();
    let (db, input) = (path(dir.path(), "db.trunkdb"), path(dir.path(), "in.jsonl"));
    std::fs::write(&input, SHAPES).unwrap();
    let imported = trunkdb(&["import", &db, &input]);
    assert!(imported.status.success(), "{}", stderr(&imported));

    let output = trunkdb(&["fields", &db, "users"]);
    assert!(output.status.success(), "{}", stderr(&output));
    // `_id` first, then names in order, a field before what's below it.
    // Every count is of documents: Ada's two string tags count once.
    assert_eq!(
        stdout(&output),
        "\
_id                            3  id 3
Zone                           1  int 1
`a.b` (no path can name it)    1  object 1
`a.b`.c (no path can name it)  1  int 1
`a``b`                         1  bool 1
address                        3  object 2, string 1
address.city                   2  string 2
age                            3  null 1, int 1, float 1
bin                            1  binary 1
`first name`                   1  string 1
grid                           2  array 1, object 1
grid.n                         1  int 1
grid[*]                        1  array 1
grid[*][*]                     1  int 1
größe                          1  int 1
name                           3  string 3
`order`                        1  int 1
seen                           1  datetime 1
tags                           3  array 3
tags[*]                        2  string 2, object 1
tags[*].k                      1  int 1
"
    );
    assert_eq!(stderr(&output), "21 paths in 3 documents\n");

    // The words after the collection are one filter, and only what it
    // finds is summed up.
    let output = trunkdb(&[
        "fields", &db, "users", "select", "name", "where", "age", ">", "40",
    ]);
    assert_eq!(stdout(&output), "_id   1  id 1\nname  1  string 1\n");
    assert_eq!(stderr(&output), "2 paths in 1 document\n");

    // Every path that can be named, pasted into a filter, finds the
    // documents it was counted in.
    let read_only = trunkdb::OpenOptions::default().read_only(true);
    let opened = trunkdb::Database::open_with(&db, read_only).unwrap();
    let users = opened.collection::<trunkdb::Document>("users");
    let all = stdout(&trunkdb(&["fields", &db, "users"]));
    for line in all.lines().filter(|line| !line.contains("no path")) {
        let mut words = line.split("  ").filter(|word| !word.is_empty());
        let (path, documents) = (words.next().unwrap(), words.next().unwrap());
        let filter: trunkdb::query::Filter = format!("exists({path})").parse().unwrap();
        assert_eq!(
            users.count(filter).unwrap().to_string(),
            documents.trim(),
            "{line}"
        );
    }
    drop(users);
    close(opened);

    let output = trunkdb(&["fields", &db, "empty"]);
    assert_eq!(
        (stdout(&output), stderr(&output)),
        (String::new(), "0 paths in 0 documents\n".to_string())
    );
}

#[test]
fn fields_names_a_mistake_as_find_does() {
    let dir = tempfile::tempdir().unwrap();
    let (db, input) = (path(dir.path(), "db.trunkdb"), path(dir.path(), "in.jsonl"));
    std::fs::write(&input, SHAPES).unwrap();
    assert!(trunkdb(&["import", &db, &input]).status.success());

    let output = trunkdb(&["fields", &db, "users", "name = Ada"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stdout(&output), "");
    assert_eq!(
        stderr(&output),
        "trunkdb: filter: expected a value, found `Ada` at line 1 column 8\n  name = Ada\n         ^\n"
    );
    let output = trunkdb(&["fields", &db, "user"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "trunkdb: user: no such collection\n");
    let missing = path(dir.path(), "typo.trunkdb");
    assert_eq!(
        trunkdb(&["fields", &missing, "users"]).status.code(),
        Some(1)
    );
    assert!(!Path::new(&missing).exists());
    assert_eq!(trunkdb(&["fields", &db]).status.code(), Some(2));
}

/// Names that are data, a field per id or per day, would grow the
/// summary without end: it keeps the first 10,000 paths, and says so.
#[test]
fn fields_keeps_at_most_ten_thousand_paths() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = path(dir.path(), "db.trunkdb");
    let db = trunkdb::Database::open(&db_path).unwrap();
    let docs = db.collection::<trunkdb::Document>("docs");
    let wide: trunkdb::Map = (0..10_000)
        .map(|n| (format!("f{n:05}"), trunkdb::Document::Int(n)))
        .collect();
    // With `_id`, 10,001 paths; the last one left out is in the second
    // document, which `_id` and `f00000` still count.
    docs.insert(trunkdb::Document::Object(wide)).unwrap();
    docs.insert(object("f00000", trunkdb::Document::Int(1)))
        .unwrap();
    drop(docs);
    close(db);

    let output = trunkdb(&["fields", &db_path, "docs"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    assert_eq!(out.lines().count(), 10_000);
    assert!(
        out.starts_with("_id     2  id 2\nf00000  2  int 2\n"),
        "{}",
        &out[..80]
    );
    assert_eq!(
        stderr(&output),
        "10000 paths in 2 documents\n\
         only the first 10000 paths found are counted; a filter can narrow it\n"
    );
}

/// `{field: value}`: every document is an object (SPEC §95).
fn object(field: &str, value: trunkdb::Document) -> trunkdb::Document {
    trunkdb::Document::Object([(field.to_string(), value)].into_iter().collect())
}

/// A file from before §125 says neither release: `info` says unknown
/// (SPEC §125.4). The 0.23.0 fixture (SPEC §120) is one, read from a
/// copy.
#[test]
fn info_says_unknown_for_a_file_from_before_the_releases() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/0.23.0/trunkdb.db");
    let db = path(dir.path(), "old.trunkdb");
    std::fs::copy(fixture, &db).unwrap();

    let output = trunkdb(&["info", &db]);
    assert!(output.status.success(), "{}", stderr(&output));
    let info = stdout(&output);
    assert!(
        info.contains("\n  written by unknown, created by unknown\n"),
        "{info}"
    );
}

/// The first `{"$id": "…"}` in `text`, as `find` prints an id.
fn id_in(text: &str) -> String {
    let at = text.find("\"$id\":\"").expect("an id") + 7;
    text[at..at + 36].to_string()
}

/// `count` prints the number alone, as `find` would find them, skip and
/// limit applied (SPEC §155).
#[test]
fn count_prints_how_many_a_filter_finds() {
    let dir = tempfile::tempdir().unwrap();
    let (db, input) = (path(dir.path(), "db.trunkdb"), path(dir.path(), "in.jsonl"));
    std::fs::write(&input, EXPORT).unwrap();
    assert!(trunkdb(&["import", &db, &input]).status.success());
    assert_eq!(stdout(&trunkdb(&["count", &db, "users"])), "2\n");
    assert_eq!(
        stdout(&trunkdb(&["count", &db, "users", "age", ">", "40"])),
        "1\n"
    );
    assert_eq!(stdout(&trunkdb(&["count", &db, "users", "limit 1"])), "1\n");
    assert_eq!(
        stdout(&trunkdb(&["count", &db, "users", "offset 5"])),
        "0\n"
    );
    assert_eq!(stdout(&trunkdb(&["count", &db, "empty"])), "0\n");
    let output = trunkdb(&["count", &db, "nobody"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("nobody: no such collection"));
    let output = trunkdb(&["count", &db, "users", "age >"]);
    assert_eq!(output.status.code(), Some(2), "a filter that doesn't read");
}

/// `get` prints the document with an id, written bare or as a filter
/// writes it; none is exit code 1, and what isn't an id 2 (SPEC §155).
#[test]
fn get_prints_the_document_with_an_id() {
    let dir = tempfile::tempdir().unwrap();
    let (db, input) = (path(dir.path(), "db.trunkdb"), path(dir.path(), "in.jsonl"));
    std::fs::write(&input, EXPORT).unwrap();
    assert!(trunkdb(&["import", &db, &input]).status.success());
    let found = stdout(&trunkdb(&["find", &db, "users", "name = \"Bob\""]));
    let id = id_in(&found);
    let output = trunkdb(&["get", &db, "users", &id]);
    assert!(output.status.success());
    assert_eq!(stdout(&output), found, "as find prints it");
    let written = format!("id(\"{id}\")");
    assert_eq!(stdout(&trunkdb(&["get", &db, "users", &written])), found);

    let missing = "0192f0c4-7d7a-7c43-9e0b-1f6d1b8f0a11";
    let output = trunkdb(&["get", &db, "users", missing]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), format!("no document {missing} in users\n"));
    let output = trunkdb(&["get", &db, "users", "Bob"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("is not an id"));
    let output = trunkdb(&["get", &db, "nobody", &id]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("nobody: no such collection"));
}

/// `--json` (SPEC §155): `info` one object with every collection, its
/// count, indexes and sizes; `fields` an object per path, in the order
/// the text has them.
#[test]
fn info_and_fields_print_json_for_a_program() {
    let dir = tempfile::tempdir().unwrap();
    let (db, input) = (path(dir.path(), "db.trunkdb"), path(dir.path(), "in.jsonl"));
    std::fs::write(&input, EXPORT).unwrap();
    assert!(trunkdb(&["import", &db, &input]).status.success());

    let output = trunkdb(&["info", &db, "--json"]);
    assert!(output.status.success());
    let info: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(info["format"], 17);
    assert_eq!(info["written_by"], trunkdb::RELEASE.to_string());
    let collections = info["collections"].as_array().unwrap();
    let users = collections.iter().find(|c| c["name"] == "users").unwrap();
    assert_eq!(users["documents"], 2);
    assert_eq!(
        users["indexes"][1],
        serde_json::json!({"fields": ["email"], "unique": true, "sparse": false})
    );
    assert_eq!(
        users["indexes"][3]["fields"],
        serde_json::json!(["team", "email"])
    );
    assert!(users["sizes"]["document_bytes"].as_u64().unwrap() > 0);
    let empty = collections.iter().find(|c| c["name"] == "empty").unwrap();
    assert_eq!(empty["documents"], 0);

    let output = trunkdb(&["fields", &db, "users", "--json"]);
    assert!(output.status.success());
    let lines: Vec<serde_json::Value> = (stdout(&output).lines())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let text = stdout(&trunkdb(&["fields", &db, "users"]));
    let paths: Vec<&str> = text
        .lines()
        .map(|l| l.split_whitespace().next().unwrap())
        .collect();
    let json_paths: Vec<&str> = lines.iter().map(|l| l["path"].as_str().unwrap()).collect();
    assert_eq!(json_paths, paths);
    let age = lines.iter().find(|l| l["path"] == "age").unwrap();
    assert_eq!(
        *age,
        serde_json::json!({"path": "age", "nameable": true, "documents": 2, "kinds": {"int": 2}})
    );
    assert_eq!(stderr(&output), "4 paths in 2 documents\n");
    // A filter, and a mistake in one, as without `--json`.
    let output = trunkdb(&["fields", &db, "users", "age > 40", "--json"]);
    assert_eq!(stderr(&output), "4 paths in 1 document\n");
    let output = trunkdb(&["fields", &db, "users", "age >", "--json"]);
    assert_eq!(output.status.code(), Some(2));
}

// Writing (SPEC §156).

/// `trunkdb` with `input` on its standard input.
fn trunkdb_with(args: &[&str], input: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_trunkdb"))
        .args(args)
        .stdin(Stdio::piped())
        .output_with(|stdin| stdin.write_all(input.as_bytes()))
}

/// The documents of `collection`, as `find` prints them, one a line.
fn all(db: &str, collection: &str) -> Vec<serde_json::Value> {
    (stdout(&trunkdb(&["find", db, collection])).lines())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// A database of the export's users, and its path.
fn users_db(dir: &Path) -> String {
    let (db, input) = (path(dir, "db.trunkdb"), path(dir, "in.jsonl"));
    std::fs::write(&input, EXPORT).unwrap();
    assert!(trunkdb(&["import", &db, &input]).status.success());
    db
}

/// One document as an argument, or one per line on standard input, all
/// in one batch; each id printed. A collection or a file that isn't
/// there takes `--create`; a line that isn't a document inserts none.
#[test]
fn insert_adds_documents_and_prints_their_ids() {
    let dir = tempfile::tempdir().unwrap();
    let db = users_db(dir.path());
    let output = trunkdb(&["insert", &db, "users", r#"{"name": "Cy", "age": 29}"#]);
    assert!(output.status.success(), "{}", stderr(&output));
    let id = stdout(&output).trim().to_string();
    assert_eq!(stderr(&output), "inserted 1 document\n");
    assert!(stdout(&trunkdb(&["get", &db, "users", &id])).contains(r#""name":"Cy""#));

    let lines =
        "{\"name\": \"Di\"}\n\n{\"name\": \"Ed\", \"at\": {\"$date\": \"2026-01-02T03:04:05Z\"}}\n";
    let output = trunkdb_with(&["insert", &db, "users", "-"], lines);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).lines().count(), 2);
    assert_eq!(stdout(&trunkdb(&["count", &db, "users"])), "5\n");
    let ed = stdout(&trunkdb(&[
        "find",
        &db,
        "users",
        r#"at = date("2026-01-02T03:04:05Z")"#,
    ]));
    assert!(ed.contains(r#""name":"Ed""#), "tags read as values: {ed}");

    // A line that isn't a document: none of them inserted.
    let output = trunkdb_with(
        &["insert", &db, "users", "-"],
        "{\"name\": \"Fa\"}\n[1, 2]\n",
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("line 2: a document is a JSON object"),
        "{}",
        stderr(&output)
    );
    let output = trunkdb(&["insert", &db, "users", "{\"name\": "]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stdout(&trunkdb(&["count", &db, "users"])), "5\n");

    // A collection, or a file, that isn't there.
    let output = trunkdb(&["insert", &db, "teams", r#"{"name": "core"}"#]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("teams: no such collection; --create makes it"));
    let output = trunkdb(&["insert", &db, "teams", r#"{"name": "core"}"#, "--create"]);
    assert!(output.status.success());
    assert_eq!(stdout(&trunkdb(&["count", &db, "teams"])), "1\n");
    let new = path(dir.path(), "new.trunkdb");
    let output = trunkdb(&["insert", &new, "notes", "{}"]);
    assert!(stderr(&output).contains("no such file"));
    assert!(!Path::new(&new).exists(), "not created without --create");
    assert!(
        trunkdb(&["insert", &new, "notes", "{}", "--create"])
            .status
            .success()
    );
    assert_eq!(stdout(&trunkdb(&["count", &new, "notes"])), "1\n");
    let output = trunkdb(&["insert", &db, "users", "{}", "--force"]);
    assert_eq!(output.status.code(), Some(2));
}

/// Every operator of an update, as MongoDB writes it, and what each
/// leaves (SPEC §156.2).
#[test]
fn update_changes_what_a_filter_finds_by_its_operators() {
    let dir = tempfile::tempdir().unwrap();
    let db = users_db(dir.path());
    let ada = r#"name = "Ada""#;
    let changes = |update: &str, changed: &str| {
        let output = trunkdb(&["update", &db, "users", ada, update]);
        assert!(output.status.success(), "{update}: {}", stderr(&output));
        assert_eq!(stderr(&output), format!("updated {changed}\n"), "{update}");
    };
    let run = |update: &str| changes(update, "1 document");
    run(r#"{"$set": {"team": "core", "tags": ["a"], "n": 3}, "$inc": {"age": 1}}"#);
    run(r#"{"$mul": {"n": 2}, "$min": {"age": 20}, "$max": {"low": 5}}"#);
    run(
        r#"{"$push": {"tags": {"$each": ["b", "c"]}}, "$addToSet": {"set": {"$each": [1, 1, 2]}}}"#,
    );
    run(r#"{"$pull": {"tags": "b"}, "$pop": {"set": -1}, "$rename": {"team": "group"}}"#);
    // In, and out again: the document as it was, so none counts as
    // updated.
    changes(
        r#"{"$push": {"tags": "d"}, "$addToSet": {"tags": "a"}, "$pop": {"tags": 1}}"#,
        "0 documents",
    );
    run(r#"{"$unset": ["low"]}"#);
    run(r#"{"$unset": {"email": ""}}"#);
    let found = stdout(&trunkdb(&["find", &db, "users", ada]));
    let doc: serde_json::Value = serde_json::from_str(found.trim()).unwrap();
    assert_eq!(doc["age"], 20);
    assert_eq!(doc["n"], 6);
    assert_eq!(doc["tags"], serde_json::json!(["a", "c"]));
    assert_eq!(doc["set"], serde_json::json!([2]));
    assert_eq!(doc["group"], "core");
    assert!(doc.get("team").is_none() && doc.get("low").is_none() && doc.get("email").is_none());

    // One operator that fails fails the whole update: nothing written.
    let output = trunkdb(&[
        "update",
        &db,
        "users",
        ada,
        r#"{"$inc": {"n": 1}, "$pop": {"name": 1}}"#,
    ]);
    assert_eq!(output.status.code(), Some(1));
    let again: serde_json::Value =
        serde_json::from_str(stdout(&trunkdb(&["find", &db, "users", ada])).trim()).unwrap();
    assert_eq!(again["n"], 6);

    // What isn't an update.
    for (update, says) in [
        (r#"{"name": "Al"}"#, "`name` isn't an operator"),
        (
            r#"{"$replace": {"a": 1}}"#,
            "$replace: not an update operator",
        ),
        ("{}", "the update has no operators"),
        ("[1]", "the update is an object of operators"),
        (r#"{"$set": 1}"#, "$set takes an object of paths"),
        (r#"{"$pop": {"tags": 2}}"#, "$pop takes 1"),
        (
            r#"{"$rename": {"a": 1}}"#,
            "$rename takes the new path as a string",
        ),
        ("{", "the update isn't JSON"),
    ] {
        let output = trunkdb(&["update", &db, "users", ada, update]);
        assert_eq!(output.status.code(), Some(2), "{update}");
        assert!(
            stderr(&output).contains(says),
            "{update}: {}",
            stderr(&output)
        );
    }
}

/// A write that would take every document, more than one, needs `--all`;
/// `--dry-run` counts and writes nothing (SPEC §156.3).
#[test]
fn update_and_delete_ask_for_all_and_can_run_dry() {
    let dir = tempfile::tempdir().unwrap();
    let db = users_db(dir.path());
    let set = r#"{"$set": {"seen": true}}"#;
    for filter in ["", "age > 0", "limit 5"] {
        let output = trunkdb(&["update", &db, "users", filter, set]);
        assert_eq!(output.status.code(), Some(2), "{filter:?}");
        assert!(stderr(&output).contains("this would update all 2 documents of users; add --all"));
        let output = trunkdb(&["delete", &db, "users", filter]);
        assert!(stderr(&output).contains("this would delete all 2 documents of users; add --all"));
    }
    let output = trunkdb(&["update", &db, "users", "", set, "--dry-run", "--all"]);
    assert_eq!(stderr(&output), "would update 2 documents\n");
    let output = trunkdb(&["delete", &db, "users", "age > 40", "--dry-run"]);
    assert_eq!(stderr(&output), "would delete 1 document\n");
    assert_eq!(
        stdout(&trunkdb(&["count", &db, "users", "seen = true"])),
        "0\n"
    );
    assert_eq!(stdout(&trunkdb(&["count", &db, "users"])), "2\n");

    let output = trunkdb(&["delete", &db, "users", "age > 40"]);
    assert_eq!(stderr(&output), "deleted 1 document\n");
    // One left: the one document is all of them, and needs no `--all`.
    let output = trunkdb(&["update", &db, "users", "", set]);
    assert_eq!(stderr(&output), "updated 1 document\n");
    let output = trunkdb(&["delete", &db, "users", "", "--all"]);
    assert_eq!(stderr(&output), "deleted 1 document\n");
    assert_eq!(stdout(&trunkdb(&["count", &db, "users"])), "0\n");
    let output = trunkdb(&["delete", &db, "users", "name =", "--all"]);
    assert_eq!(output.status.code(), Some(2), "a filter that doesn't read");
    let output = trunkdb(&["delete", &db, "nobody", ""]);
    assert!(stderr(&output).contains("nobody: no such collection"));
}

/// `--dry-run` rehearses the write the run would make, as a batch's
/// dry run does (SPEC §184.2): the errors the run would fail with, the
/// documents it would change and not those it would leave, and an upsert
/// with the run's own filter. Nothing is written either way.
#[test]
fn a_dry_run_rehearses_the_write() {
    let dir = tempfile::tempdir().unwrap();
    let db = users_db(dir.path());
    let before = std::fs::read(&db).unwrap();
    let fails = |args: &[&str], says: &str| {
        let dry: Vec<&str> = args.iter().copied().chain(["--dry-run"]).collect();
        for args in [&dry[..], args] {
            let output = trunkdb(args);
            assert_eq!(
                output.status.code(),
                Some(1),
                "{args:?}: {}",
                stderr(&output)
            );
            assert!(
                stderr(&output).contains(says),
                "{args:?}: {}",
                stderr(&output)
            );
        }
    };
    // A unique index broken; an operator that can't be made.
    fails(
        &[
            "update",
            &db,
            "users",
            "age > 40",
            r#"{"$set": {"email": "a@x"}}"#,
        ],
        "email",
    );
    fails(
        &[
            "update",
            &db,
            "users",
            r#"name = "Ada""#,
            r#"{"$inc": {"name": 1}}"#,
        ],
        "name",
    );
    // An upsert that finds two; one whose filter pages, which an upsert
    // doesn't: refused, where the count said "insert" and the run updated.
    fails(
        &[
            "update",
            &db,
            "users",
            "age > 0",
            r#"{"$set": {"x": 1}}"#,
            "--upsert",
        ],
        "2 documents match the filter",
    );
    fails(
        &[
            "update",
            &db,
            "users",
            r#"name = "Ada" offset 1"#,
            r#"{"$set": {"x": 1}}"#,
            "--upsert",
        ],
        "an upsert's filter can't have a skip",
    );
    assert_eq!(std::fs::read(&db).unwrap(), before, "nothing written");

    // Only what changes counts: Ada is 36 already.
    let set = r#"{"$set": {"age": 36}}"#;
    let output = trunkdb(&["update", &db, "users", "", set, "--all", "--dry-run"]);
    assert_eq!(stderr(&output), "would update 1 document\n");
    let upsert = ["update", &db, "users", r#"name = "Ada""#, set, "--upsert"];
    let dry: Vec<&str> = upsert.iter().copied().chain(["--dry-run"]).collect();
    assert_eq!(stderr(&trunkdb(&dry)), "would leave 1 document as it was\n");
    assert_eq!(std::fs::read(&db).unwrap(), before, "nothing written");
    let output = trunkdb(&upsert);
    assert_eq!(stderr(&output), "left 1 document as it was\n");
    let output = trunkdb(&["update", &db, "users", "", set, "--all"]);
    assert_eq!(stderr(&output), "updated 1 document\n");
}

/// `--upsert`: the one match updated, or a document of the filter's `=`s
/// and the update inserted, with `$setOnInsert`.
#[test]
fn update_upserts_where_nothing_matches() {
    let dir = tempfile::tempdir().unwrap();
    let db = users_db(dir.path());
    let update = r#"{"$inc": {"visits": 1}, "$setOnInsert": {"new": true}}"#;
    let output = trunkdb(&[
        "update",
        &db,
        "users",
        r#"name = "Zed""#,
        update,
        "--upsert",
        "--dry-run",
    ]);
    assert_eq!(stderr(&output), "would insert 1 document\n");
    let output = trunkdb(&[
        "update",
        &db,
        "users",
        r#"name = "Zed""#,
        update,
        "--upsert",
    ]);
    assert_eq!(stderr(&output), "inserted 1 document\n");
    let id = stdout(&output).trim().to_string();
    let output = trunkdb(&[
        "update",
        &db,
        "users",
        r#"name = "Zed""#,
        update,
        "--upsert",
    ]);
    assert_eq!(stderr(&output), "updated 1 document\n");
    assert_eq!(stdout(&output).trim(), id);
    let zed: serde_json::Value =
        serde_json::from_str(stdout(&trunkdb(&["get", &db, "users", &id])).trim()).unwrap();
    assert_eq!(
        (
            zed["name"].clone(),
            zed["visits"].clone(),
            zed["new"].clone()
        ),
        ("Zed".into(), 2.into(), true.into())
    );
}

/// Indexes added, found there already, refused where a unique one finds
/// two equal, dropped; a collection dropped.
#[test]
fn indexes_and_collections_are_added_and_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let db = users_db(dir.path());
    let output = trunkdb(&["index", &db, "users", "add", "name", "age", "--unique"]);
    assert_eq!(stderr(&output), "added index (name, age) to users\n");
    let output = trunkdb(&["index", &db, "users", "add", "name", "age", "--unique"]);
    assert_eq!(stderr(&output), "users has index (name, age) already\n");
    let output = trunkdb(&["index", &db, "users", "add", "city", "--sparse"]);
    assert!(output.status.success());
    let info = stdout(&trunkdb(&["info", &db]));
    assert!(
        info.contains("(name, age) (unique)") && info.contains("city (sparse)"),
        "{info}"
    );
    // Two documents with no `team` collide in a unique index that isn't sparse.
    assert!(
        trunkdb(&["insert", &db, "users", r#"{"name": "Ada", "age": 99}"#])
            .status
            .success()
    );
    let output = trunkdb(&["index", &db, "users", "add", "name", "--unique"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(!stdout(&trunkdb(&["info", &db])).contains("name (unique)"));

    let output = trunkdb(&["index", &db, "users", "drop", "name", "age"]);
    assert_eq!(stderr(&output), "dropped index (name, age) of users\n");
    let output = trunkdb(&["index", &db, "users", "drop", "name", "age"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("users has no index (name, age)"));
    let output = trunkdb(&["index", &db, "users", "add"]);
    assert_eq!(output.status.code(), Some(2));
    let output = trunkdb(&["index", &db, "nobody", "add", "x"]);
    assert!(stderr(&output).contains("nobody: no such collection"));

    let output = trunkdb(&["drop", &db, "users"]);
    assert_eq!(stderr(&output), "dropped users and its 3 documents\n");
    assert!(!stdout(&trunkdb(&["info", &db])).contains("users:"));
    let output = trunkdb(&["drop", &db, "users"]);
    assert_eq!(output.status.code(), Some(1));
}

/// A file another program has open for writing: every write refused as
/// in use, nothing changed (SPEC §156, §185.6).
#[test]
fn writes_refuse_a_file_in_use() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = users_db(dir.path());
    let held = trunkdb::Database::open(&db_path).unwrap();
    for args in [
        &["insert", &db_path, "users", "{}"][..],
        &[
            "update",
            &db_path,
            "users",
            "age > 40",
            r#"{"$set": {"a": 1}}"#,
        ],
        &["delete", &db_path, "users", "age > 40"],
        &["index", &db_path, "users", "add", "x"],
        &["drop", &db_path, "users"],
    ] {
        let output = trunkdb(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(
            stderr(&output).contains("in use by another program"),
            "{args:?}"
        );
    }
    close(held);
    assert_eq!(all(&db_path, "users").len(), 2);
}

// Aggregates as text (SPEC §157).

/// The rows of an aggregate over what a filter finds, one JSON line
/// each; `--explain` says how; a mistake in either text is shown where it
/// is, exit code 2.
#[test]
fn aggregate_prints_the_rows_of_its_text() {
    let dir = tempfile::tempdir().unwrap();
    let db = users_db(dir.path());
    let lines = "{\"name\": \"Cy\", \"age\": 29, \"team\": \"web\", \"tags\": [\"a\", \"b\"]}\n\
                 {\"name\": \"Di\", \"age\": 52, \"team\": \"core\", \"tags\": [\"b\"]}\n";
    assert!(
        trunkdb_with(&["insert", &db, "users", "-"], lines)
            .status
            .success()
    );
    let rows = |filter: &str, aggregate: &str| -> Vec<serde_json::Value> {
        let output = trunkdb(&["aggregate", &db, "users", filter, aggregate]);
        assert!(output.status.success(), "{aggregate}: {}", stderr(&output));
        (stdout(&output).lines())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    };
    assert_eq!(
        rows(
            "",
            "count() as n, max(age) as oldest group by team order by n desc, team"
        ),
        [
            serde_json::json!({"team": null, "n": 2, "oldest": 41}),
            serde_json::json!({"team": "core", "n": 1, "oldest": 52}),
            serde_json::json!({"team": "web", "n": 1, "oldest": 29}),
        ]
    );
    assert_eq!(
        rows(
            "age >= 30",
            "count() as n, count() filter (where age > 40) as over, avg(age) as mean"
        ),
        [serde_json::json!({"n": 3, "over": 2, "mean": 43.0})]
    );
    assert_eq!(
        rows("", "unwind tags count() as n group by tags having n > 1"),
        [serde_json::json!({"tags": "b", "n": 2})]
    );
    assert_eq!(
        rows(
            "",
            "last(name order by age) as eldest, count(distinct team) as teams"
        ),
        [serde_json::json!({"eldest": "Di", "teams": 2})]
    );
    let output = trunkdb(&["aggregate", &db, "users", "", "count() as n group by team"]);
    assert_eq!(stderr(&output), "3 rows\n");

    let output = trunkdb(&[
        "aggregate",
        &db,
        "users",
        "age > 30",
        "count() as n",
        "--explain",
    ]);
    assert!(output.status.success());
    assert!(
        stdout(&output).contains("documents returned: 1"),
        "{}",
        stdout(&output)
    );

    let output = trunkdb(&["aggregate", &db, "users", "", "count() n"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        stderr(&output),
        "trunkdb: aggregate: expected `as` and the column's name, found `n` at line 1 column 9\n  count() n\n          ^\n"
    );
    let output = trunkdb(&["aggregate", &db, "users", "age >", "count() as n"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).starts_with("trunkdb: filter:"),
        "{}",
        stderr(&output)
    );
    let output = trunkdb(&["aggregate", &db, "nobody", "", "count() as n"]);
    assert!(stderr(&output).contains("nobody: no such collection"));
    // Refused as the library refuses it, before anything is read.
    let output = trunkdb(&["aggregate", &db, "users", "", "count() as n, sum(age) as n"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
}

/// SPEC §163.6: `--param name=value` for each command that reads a
/// filter or an aggregate, the value as JSON, `date(…)` or a word, never
/// read as text; each mistake exit code 2.
#[test]
fn parameters_are_given_apart_from_the_text() {
    let dir = tempfile::tempdir().unwrap();
    let (db, input) = (path(dir.path(), "db.trunkdb"), path(dir.path(), "in.jsonl"));
    std::fs::write(&input, EXPORT).unwrap();
    assert!(trunkdb(&["import", &db, &input]).status.success());
    let sneaky = r#"{"name": "x\" or age > 0 or name = \"y", "age": 7, "code": "20", "when": {"$date": "2026-11-03T00:00:00Z"}, "day": {"$date": "2026-11-03"}, "at": {"$time": "09:30:00"}}"#;
    assert!(trunkdb(&["insert", &db, "users", sneaky]).status.success());
    let ok = |args: &[&str]| -> String {
        let output = trunkdb(args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        stdout(&output)
    };
    let refused = |args: &[&str]| -> String {
        let output = trunkdb(args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            stdout(&output)
        );
        assert!(stdout(&output).is_empty(), "{args:?}");
        stderr(&output)
    };

    // A word is a string; JSON a number; any string compared as itself.
    let found = ok(&["find", &db, "users", "name = :n", "--param", "n=Ada"]);
    assert!(
        found.contains(r#""name":"Ada""#) && found.lines().count() == 1,
        "{found}"
    );
    let count = |args: &[&str]| ok(args).trim().to_string();
    assert_eq!(
        count(&["count", &db, "users", "age >= :a", "--param", "a=40"]),
        "1"
    );
    let name = r#"n=x" or age > 0 or name = "y"#;
    assert_eq!(
        count(&["count", &db, "users", "name = :n", "--param", name]),
        "1"
    );
    assert_eq!(
        count(&["count", &db, "users", "code = :c", "--param", "c=20"]),
        "0"
    );
    assert_eq!(
        count(&["count", &db, "users", "code = :c", "--param", r#"c="20""#]),
        "1"
    );
    // A moment, written either way; a day; a time of day (SPEC §173),
    // each matching only its own kind.
    for (filter, value, n) in [
        ("when = :w", r#"w=date("2026-11-03T00:00:00Z")"#, "1"),
        ("when = :w", r#"w=datetime("2026-11-03T00:00:00Z")"#, "1"),
        ("when = :w", r#"w=date("2026-11-03")"#, "0"),
        ("day = :w", r#"w=date("2026-11-03")"#, "1"),
        ("day = :w", r#"w=datetime("2026-11-03T00:00:00Z")"#, "0"),
        ("at < :w", r#"w=time("10:00")"#, "1"),
        ("at < :w", r#"w=time("09:30")"#, "0"),
    ] {
        let found = count(&["count", &db, "users", filter, "--param", value]);
        assert_eq!(found, n, "{filter} {value}");
    }
    let fields = ok(&["fields", &db, "users"]);
    for (path, kind) in [
        ("at ", "time 1"),
        ("day ", "date 1"),
        ("when ", "datetime 1"),
    ] {
        let line = fields.lines().find(|l| l.starts_with(path)).unwrap();
        assert!(line.ends_with(kind), "{line}");
    }
    let list = r#"l=["Ada", "Bob", "Zed"]"#;
    assert_eq!(
        count(&["count", &db, "users", "name in :l", "--param", list]),
        "2"
    );
    let paged = [
        "count",
        &db,
        "users",
        "age > :a limit :k",
        "--param",
        "a=0",
        "--param",
        "k=2",
    ];
    assert_eq!(count(&paged), "2");
    // Anywhere among the arguments, and with --explain after.
    let explained = ok(&[
        "find",
        &db,
        "--param",
        "a=40",
        "users",
        "age > :a",
        "--explain",
    ]);
    assert!(explained.contains("documents returned: 1"), "{explained}");
    let fields = ok(&[
        "fields", &db, "users", "age < :a", "--param", "a=10", "--json",
    ]);
    assert!(fields.contains("code"), "{fields}");

    // The aggregate's text and the filter's share the values.
    let rows = ok(&[
        "aggregate",
        &db,
        "users",
        "age > :min",
        "count() as n having n >= :k",
        "--param",
        "min=0",
        "--param",
        "k=3",
    ]);
    assert_eq!(rows, "{\"n\":3}\n");

    // Writes: the filter's parameters, and the counts they'd change.
    let update = [
        "update",
        &db,
        "users",
        "name = :n",
        r#"{"$inc": {"age": 1}}"#,
        "--param",
        "n=Bob",
    ];
    assert!(trunkdb(&update).status.success());
    assert_eq!(count(&["count", &db, "users", "age = 42"]), "1");
    let delete = [
        "delete",
        &db,
        "users",
        "age < :a",
        "--param",
        "a=10",
        "--dry-run",
    ];
    let output = trunkdb(&delete);
    assert_eq!(stderr(&output), "would delete 1 document\n");

    // Mistakes.
    let e = refused(&["find", &db, "users", "age > :a"]);
    assert_eq!(
        e,
        "trunkdb: filter: no value for the parameter :a: give it with --param a=…\n  age > :a\n        ^\n"
    );
    let e = refused(&[
        "find", &db, "users", "age > :a", "--param", "a=1", "--param", "b=2",
    ]);
    assert_eq!(
        e,
        "trunkdb: --param b: no text of this command has the parameter :b\n"
    );
    let e = refused(&["find", &db, "users", "age > 1 limit :n", "--param", "n=-1"]);
    assert!(
        e.contains(":n is a count") && e.contains("\n                ^"),
        "{e}"
    );
    let e = refused(&["find", &db, "users", "name in :l", "--param", "l=Ada"]);
    assert!(e.contains("its value must be an array"), "{e}");
    let e = refused(&["find", &db, "users", "age > :a", "--param", "a"]);
    assert!(e.contains("--param takes a name and a value"), "{e}");
    let e = refused(&["find", &db, "users", "age > :a", "--param"]);
    assert!(e.contains("--param takes a name and a value"), "{e}");
    let twice = [
        "find", &db, "users", "age > :a", "--param", "a=1", "--param", "a=2",
    ];
    assert_eq!(refused(&twice), "trunkdb: --param a: given twice\n");
    for (value, says) in [
        ("a=[1, 2", "starts as JSON and isn't"),
        (r#"a="open"#, "starts as JSON and isn't"),
        (r#"a=date("2026-13-01")"#, "is not a date like"),
        (r#"a=datetime("2026-11-03")"#, "is not a date-time"),
        (r#"a=time("25:00")"#, "is not a time of day"),
        (r#"a=id("nope")"#, "is not an id"),
    ] {
        let e = refused(&["find", &db, "users", "age > :a", "--param", value]);
        assert!(e.contains(says), "{value}: {e}");
    }
    for command in [
        vec!["get", &db, "users", "x", "--param", "a=1"],
        vec!["info", &db, "--param", "a=1"],
        vec!["insert", &db, "users", "{}", "--param", "a=1"],
    ] {
        let e = refused(&command);
        assert_eq!(
            e, "trunkdb: --param: not an option of this command\n",
            "{command:?}"
        );
    }
    let e = refused(&[
        "find",
        &db,
        "users",
        "name = :n order by :n",
        "--param",
        "n=Ada",
    ]);
    assert!(e.contains("a parameter can't stand for a field"), "{e}");
}

/// SPEC §164.4: an OR of ids and an indexed condition, in words.
#[test]
fn explain_names_ids_looked_up_beside_an_index() {
    let dir = tempfile::tempdir().unwrap();
    let (db, input) = (path(dir.path(), "db.trunkdb"), path(dir.path(), "in.jsonl"));
    std::fs::write(&input, EXPORT).unwrap();
    assert!(trunkdb(&["import", &db, &input]).status.success());
    let ada = stdout(&trunkdb(&["find", &db, "users", r#"name = "Ada""#]));
    let doc: trunkdb::Document = ada.trim().parse().unwrap();
    let Some(trunkdb::Document::Id(id)) = doc.get("_id") else {
        panic!("{ada}")
    };
    let filter = format!(r#"_id = id("{id}") or age > 40"#);
    let output = trunkdb(&["find", &db, "users", &filter, "--explain"]);
    let explained = stdout(&output);
    assert!(
        explained.starts_with("plan: ids looked up, and the index on age, together\n"),
        "{explained}"
    );
    assert!(explained.contains("documents returned: 2"), "{explained}");
}

/// SPEC §165.2: `trunkdb verify`, an export restored, checked, counted
/// and its temporary file gone; exit code 1 and why for one that
/// doesn't restore, a note for one without an end line.
#[test]
fn verify_says_whether_an_export_restores() {
    let dir = tempfile::tempdir().unwrap();
    let (db, input, out) = (
        path(dir.path(), "db.trunkdb"),
        path(dir.path(), "in.jsonl"),
        path(dir.path(), "out.jsonl"),
    );
    std::fs::write(&input, EXPORT).unwrap();
    assert!(trunkdb(&["import", &db, &input]).status.success());
    assert!(trunkdb(&["export", &db, &out]).status.success());

    let output = trunkdb(&["verify", &out]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!("verified {out}: 2 collections, 2 documents, 4 indexes\n")
    );
    assert_eq!(stderr(&output), "");

    // From standard input.
    let exported = std::fs::read_to_string(&out).unwrap();
    let piped = Command::new(env!("CARGO_BIN_EXE_trunkdb"))
        .args(["verify", "-"])
        .stdin(Stdio::piped())
        .output_with(|stdin| stdin.write_all(exported.as_bytes()));
    assert!(piped.status.success(), "{}", stderr(&piped));
    assert!(stdout(&piped).starts_with("verified -: 2 collections"));

    // Cut short: exit code 1, the line, and nothing on standard output.
    let lines: Vec<&str> = exported.lines().collect();
    let cut = path(dir.path(), "cut.jsonl");
    std::fs::write(&cut, lines[..lines.len() - 1].join("\n") + "\n").unwrap();
    let output = trunkdb(&["verify", &cut]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert_eq!(
        stderr(&output),
        format!(
            "trunkdb: {cut}: doesn't restore: line {}: the export ends before its end line: cut short?\n",
            lines.len()
        )
    );

    // Version 1, the test's own export: restores, with a note.
    let output = trunkdb(&["verify", &input]);
    assert!(output.status.success());
    assert!(stdout(&output).starts_with(&format!("verified {input}:")));
    assert!(
        stderr(&output).contains("can't be told"),
        "{}",
        stderr(&output)
    );

    // A file that isn't there, and a call without one.
    let output = trunkdb(&["verify", &path(dir.path(), "none.jsonl")]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(trunkdb(&["verify"]).status.code(), Some(2));

    // No restore left behind in the temporary directory.
    let left: Vec<_> = std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("trunkdb-verify-")
        })
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

/// A file of tasks and a log, for `trunkdb batch` (SPEC §168).
fn tasks_db(dir: &Path) -> String {
    let db = path(dir, "tasks.trunkdb");
    let tasks = "{\"title\": \"a\", \"due\": 1, \"done\": false}\n\
                 {\"title\": \"b\", \"due\": 2, \"done\": true}\n\
                 {\"title\": \"c\", \"due\": 9, \"done\": false}\n";
    let made = trunkdb_with(&["insert", &db, "tasks", "-", "--create"], tasks);
    assert!(made.status.success(), "{}", stderr(&made));
    assert!(
        trunkdb(&["insert", &db, "log", r#"{"what": "start"}"#, "--create"])
            .status
            .success()
    );
    db
}

const MARK_OVERDUE: &str = r#"{"op": "require", "collection": "tasks", "filter": "done = false"}
{"op": "update", "collection": "tasks", "filter": "due < :now and done = false", "update": {"$set": {"overdue": true}}}
{"op": "insert", "collection": "log", "document": {"what": "marked", "at": {"$date": "2026-11-06T12:00:00Z"}}}

{"op": "delete", "collection": "tasks", "filter": "done = true"}
"#;

/// SPEC §168: one op a line, from a file or standard input, committed
/// whole; what each line wrote, the new ids on standard output, and a
/// dry run that says so and writes nothing.
#[test]
fn batch_commits_every_line_or_none() {
    let dir = tempfile::tempdir().unwrap();
    let db = tasks_db(dir.path());
    let ops = path(dir.path(), "ops.jsonl");
    std::fs::write(&ops, MARK_OVERDUE).unwrap();

    let output = trunkdb(&["batch", &db, &ops, "--dry-run", "--param", "now=5"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    assert_eq!(
        stderr(&output),
        "line 2: would update 1 document\nline 5: would delete 1 document\n\
         4 ops would insert 1 document, update 1 document and delete 1 document; nothing written\n"
    );
    assert_eq!(
        stdout(&trunkdb(&["count", &db, "tasks", "overdue = true"])),
        "0\n"
    );
    assert_eq!(stdout(&trunkdb(&["count", &db, "log"])), "1\n");

    let output = trunkdb(&["batch", &db, &ops, "--param", "now=5"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stderr(&output),
        "line 2: updated 1 document\nline 5: deleted 1 document\n\
         committed 4 ops: inserted 1 document, updated 1 document and deleted 1 document\n"
    );
    let id = stdout(&output).trim().to_string();
    let logged = stdout(&trunkdb(&["get", &db, "log", &id]));
    assert!(logged.contains(r#""what":"marked""#), "{logged}");
    assert!(
        logged.contains(r#""$date""#),
        "tags read as values: {logged}"
    );
    assert_eq!(
        stdout(&trunkdb(&["count", &db, "tasks", "overdue = true"])),
        "1\n"
    );
    assert_eq!(stdout(&trunkdb(&["count", &db, "tasks"])), "2\n");

    // From standard input: every task marked done, and a precondition
    // after it that a done task breaks; the whole batch writes nothing.
    let output = trunkdb_with(
        &["batch", &db, "-"],
        r#"{"op": "update", "collection": "tasks", "filter": "", "all": true, "update": {"$set": {"done": true}}}
{"op": "insert", "collection": "log", "document": {"what": "all done"}}
{"op": "require_none", "collection": "tasks", "filter": "done = true"}
"#,
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).starts_with("trunkdb: line 3: op 2 of the batch, a precondition"),
        "{}",
        stderr(&output)
    );
    assert!(stderr(&output).ends_with("; nothing written\n"));
    assert_eq!(stdout(&output), "", "no id of an insert rolled back");
    assert_eq!(
        stdout(&trunkdb(&["count", &db, "tasks", "done = true"])),
        "0\n"
    );
    assert_eq!(stdout(&trunkdb(&["count", &db, "log"])), "2\n");
    assert!(trunkdb(&["check", &db]).status.success());
}

/// A failure at the commit names the line it came from, whatever the
/// error: a unique index refusing an insert, an operator that can't be
/// made to a document found.
#[test]
fn batch_names_the_line_a_commit_failed_at() {
    let dir = tempfile::tempdir().unwrap();
    let db = tasks_db(dir.path());
    assert!(
        trunkdb(&["index", &db, "log", "add", "what", "--unique"])
            .status
            .success()
    );
    let insert = |what: &str| {
        format!(r#"{{"op": "insert", "collection": "log", "document": {{"what": "{what}"}}}}"#)
    };
    let update = r#"{"op": "update", "collection": "tasks", "filter": "due > 0", "update": {"$inc": {"due": 1}}}"#;
    let mut lines = Vec::new();
    for n in 0..9 {
        lines.push(insert(&format!("n{n}")));
        lines.push(update.to_string());
    }
    lines.insert(13, insert("n2")); // line 14: n2 again
    let output = trunkdb_with(&["batch", &db, "-"], &(lines.join("\n") + "\n"));
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).starts_with("trunkdb: line 14: collection \"log\" has a unique index"),
        "{}",
        stderr(&output)
    );

    let output = trunkdb_with(
        &["batch", &db, "-"],
        &format!(
            "{}\n{}\n",
            insert("x"),
            r#"{"op": "update", "collection": "tasks", "filter": "title = \"a\"", "update": {"$inc": {"title": 1}}}"#
        ),
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).starts_with("trunkdb: line 2: "),
        "{}",
        stderr(&output)
    );
    assert_eq!(stdout(&trunkdb(&["count", &db, "log"])), "1\n");
    assert_eq!(stdout(&trunkdb(&["count", &db, "tasks", "due = 1"])), "1\n");
}

/// SPEC §175: an upsert line, its id printed with the inserts' in the
/// order of the lines, what it wrote on its line, and a dry run.
#[test]
fn batch_upserts_print_their_ids() {
    let dir = tempfile::tempdir().unwrap();
    let db = tasks_db(dir.path());
    let lines = r#"{"op": "upsert", "collection": "counts", "filter": "page = \"/home\"", "update": {"$inc": {"hits": 1}}}
{"op": "insert", "collection": "log", "document": {"what": "visit"}}
{"op": "upsert", "collection": "counts", "filter": "page = \"/home\"", "update": {"$inc": {"hits": 1}}}
{"op": "upsert", "collection": "tasks", "filter": "title = \"a\"", "update": {"$set": {"due": 1}}}
"#;
    let output = trunkdb_with(&["batch", &db, "-", "--create"], lines);
    assert!(output.status.success(), "{}", stderr(&output));
    let ids: Vec<String> = stdout(&output).lines().map(String::from).collect();
    assert_eq!(ids.len(), 4, "{ids:?}");
    assert_eq!(ids[0], ids[2], "the second upsert found the first's");
    assert_ne!(ids[0], ids[1]);
    assert_eq!(
        stderr(&output),
        "line 1: inserted 1 document\nline 3: updated 1 document\nline 4: updated 0 documents\n\
         committed 4 ops: inserted 2 documents and updated 1 document\n"
    );
    let counted = stdout(&trunkdb(&["get", &db, "counts", &ids[0]]));
    assert!(counted.contains(r#""page":"/home","hits":2"#), "{counted}");
    let a = stdout(&trunkdb(&["get", &db, "tasks", &ids[3]]));
    assert!(a.contains(r#""title":"a""#), "the task found: {a}");

    let output = trunkdb_with(&["batch", &db, "-", "--dry-run"], lines);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "", "no ids on a dry run");
    assert_eq!(
        stderr(&output),
        "line 1: would update 1 document\nline 3: would update 1 document\n\
         line 4: would update 0 documents\n\
         4 ops would insert 1 document and update 2 documents; nothing written\n"
    );

    // An empty filter is the collection's one document: no `all` needed.
    let settings = r#"{"op": "upsert", "collection": "settings", "filter": "", "update": {"$set": {"theme": "dark"}}}
"#;
    for _ in 0..2 {
        let output = trunkdb_with(&["batch", &db, "-", "--create"], settings);
        assert!(output.status.success(), "{}", stderr(&output));
    }
    assert_eq!(stdout(&trunkdb(&["count", &db, "settings"])), "1\n");

    // Several matches fail the batch at the upsert's line.
    let output = trunkdb_with(
        &["batch", &db, "-"],
        r#"{"op": "insert", "collection": "log", "document": {"what": "x"}}
{"op": "upsert", "collection": "tasks", "filter": "done = false", "update": {"$set": {"seen": true}}}
"#,
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).starts_with("trunkdb: line 2: upsert into \"tasks\": 2 documents match"),
        "{}",
        stderr(&output)
    );
    assert_eq!(stdout(&output), "");
    assert_eq!(stdout(&trunkdb(&["count", &db, "log"])), "2\n");
}

/// Mistakes in the lines are found before anything is opened for
/// writing, named with their line, exit code 2: nothing written.
#[test]
fn batch_names_a_mistake_in_a_line() {
    let dir = tempfile::tempdir().unwrap();
    let db = tasks_db(dir.path());
    let refused = |lines: &str, says: &str| {
        let output = trunkdb_with(&["batch", &db, "-"], lines);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{lines}: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains(says),
            "{lines}: {}",
            stderr(&output)
        );
    };
    let ok = r#"{"op": "insert", "collection": "log", "document": {}}"#;
    refused(&format!("{ok}\n{{\"op\": \n"), "line 2: not a JSON object");
    refused("[1]\n", "line 1: an op is a JSON object");
    refused(r#"{"collection": "log"}"#, "line 1: no `op`");
    refused(
        r#"{"op": "replace", "collection": "log"}"#,
        "replace: not an op; one of insert, update, delete, upsert, require, require_none",
    );
    refused(
        r#"{"op": "upsert", "collection": "log", "filter": "a = 1"}"#,
        "upsert: no `update`",
    );
    refused(
        r#"{"op": "upsert", "collection": "log", "filter": "a = 1 limit 1", "update": {"$set": {"b": 1}}}"#,
        "line 1: an upsert's filter can't have a limit",
    );
    refused(
        r#"{"op": "upsert", "collection": "log", "filter": "a = 1 and a = 2", "update": {"$set": {"b": 1}}}"#,
        "line 1: ",
    );
    refused(
        r#"{"op": "upsert", "collection": "log", "filter": "a = 1", "update": {"$set": {"b": 1}}, "all": true}"#,
        "`all`: not a field of an op line",
    );
    refused(
        r#"{"op": "insert", "document": {}}"#,
        "insert: no `collection`",
    );
    refused(
        r#"{"op": "insert", "collection": "log", "document": 1}"#,
        "`document` is a JSON object",
    );
    refused(
        r#"{"op": "insert", "collection": "log", "document": {}, "filter": ""}"#,
        "insert: takes no `filter`",
    );
    refused(
        r#"{"op": "delete", "collection": "log"}"#,
        "delete: no `filter`",
    );
    refused(
        r#"{"op": "delete", "collection": "log", "filter": " "}"#,
        "the filter is empty, which takes every document of log; add \"all\": true",
    );
    refused(
        r#"{"op": "delete", "collection": "log", "filter": "a = 1", "limit": 1}"#,
        "`limit`: not a field of an op line",
    );
    refused(
        r#"{"op": "delete", "collection": "log", "filter": "a = "}"#,
        "line 1: filter: expected a value",
    );
    refused(
        r#"{"op": "update", "collection": "log", "filter": "a = 1"}"#,
        "update: no `update`",
    );
    refused(
        r#"{"op": "update", "collection": "log", "filter": "a = 1", "update": {"a": 1}}"#,
        "`a` isn't an operator",
    );
    refused(
        r#"{"op": "require", "collection": "log", "filter": "limit 1"}"#,
        "line 1: a precondition's filter can't have a limit",
    );
    refused(
        r#"{"op": "require", "collection": "log", "filter": "a = :a"}"#,
        "line 1: filter: no value for the parameter :a: give it with --param a=…",
    );
    let output = trunkdb_with(&["batch", &db, "-", "--param", "b=1"], &format!("{ok}\n"));
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("--param b: no filter of the batch has the parameter :b"));
    assert_eq!(stdout(&trunkdb(&["count", &db, "log"])), "1\n");

    let output = trunkdb_with(&["batch", &db, "-"], "\n\n");
    assert!(output.status.success());
    assert_eq!(stderr(&output), "no ops; nothing written\n");
    let output = trunkdb(&["batch", &db, &path(dir.path(), "none.jsonl")]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(trunkdb(&["batch", &db]).status.code(), Some(2));
    assert_eq!(
        trunkdb(&["batch", &db, "-", "--all"]).status.code(),
        Some(2)
    );
}

/// A collection a line names must be there, or `--create` makes it; a
/// file that isn't there takes `--create` too.
#[test]
fn batch_makes_collections_only_with_create() {
    let dir = tempfile::tempdir().unwrap();
    let db = tasks_db(dir.path());
    let lines = r#"{"op": "insert", "collection": "notes", "document": {"a": 1}}
{"op": "delete", "collection": "drafts", "filter": "a = 1"}
"#;
    let output = trunkdb_with(&["batch", &db, "-"], lines);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("line 1: notes: no such collection; --create makes it"),
        "{}",
        stderr(&output)
    );
    let output = trunkdb_with(&["batch", &db, "-", "--create"], lines);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&trunkdb(&["count", &db, "notes"])), "1\n");

    let new = path(dir.path(), "new.trunkdb");
    let output = trunkdb_with(&["batch", &new, "-"], lines);
    assert!(stderr(&output).contains("no such file"));
    assert!(!Path::new(&new).exists());
    let output = trunkdb_with(&["batch", &new, "-", "--create"], lines);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&trunkdb(&["count", &new, "notes"])), "1\n");
}

/// SPEC §169: `matches` in the command's filters, the pattern a
/// parameter too, and a pattern that isn't a regex marked where it is.
#[cfg(feature = "regex")]
#[test]
fn find_takes_a_regex() {
    let dir = tempfile::tempdir().unwrap();
    let db = tasks_db(dir.path());
    let output = trunkdb(&["count", &db, "tasks", r#"title matches "^[ab]$""#]);
    assert_eq!(stdout(&output), "2\n", "{}", stderr(&output));
    let output = trunkdb(&[
        "count",
        &db,
        "tasks",
        "title matches :p",
        "--param",
        "p=(?i)^C",
    ]);
    assert_eq!(stdout(&output), "1\n", "{}", stderr(&output));
    let output = trunkdb(&["find", &db, "tasks", r#"title matches "(a""#]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("not a regex"),
        "{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("\n                ^"),
        "{}",
        stderr(&output)
    );
    let output = trunkdb(&["find", &db, "tasks", "title matches :p", "--param", "p=(a"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("the parameter :p is a regex, after `matches`: not a regex"),
        "{}",
        stderr(&output)
    );
}
