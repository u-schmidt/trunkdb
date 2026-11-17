//! What an open database says about itself (SPEC §208): the file it
//! works on, how it was opened, and the options it was opened with.

use std::path::Path;
use std::time::Duration;
use trunkdb::{Database, Document, OpenMode, OpenOptions};

fn dir_and_path() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("info.trunkdb");
    (dir, path)
}

/// The WAL beside the file at `path`.
fn wal(path: &Path) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".wal");
    name.into()
}

/// A writer: the path given, its WAL beside it, not read-only, not
/// poisoned; every clone says the same.
#[test]
fn a_writer_says_its_file_and_mode() {
    let (_dir, path) = dir_and_path();
    let db = Database::open(&path).unwrap();
    assert_eq!(db.path(), path);
    assert!(!db.is_read_only());
    let info = db.open_info();
    assert_eq!(info.path, path);
    assert_eq!(info.wal_path, wal(&path));
    assert_eq!(info.mode, OpenMode::Writer);
    assert!(!info.poisoned);
    let clone = db.clone();
    assert_eq!(clone.open_info(), info);
}

/// The options set come back, and those not set as the open uses them:
/// the WAL's bytes from the pages, and four times them for letting
/// readers go.
#[test]
fn the_options_come_back_as_used() {
    let (_dir, path) = dir_and_path();
    let db = Database::open_with(
        &path,
        OpenOptions::default()
            .cache_size(1 << 20)
            .checkpoint_pages(100)
            .snapshot_memory(3 << 20)
            .checkpoint_wait(Duration::from_millis(7))
            .wait_for_readers(Duration::from_millis(9))
            .watch_every(Duration::from_millis(250))
            .clock_every(Duration::from_millis(1500)),
    )
    .unwrap();
    let options = db.open_info().options;
    let page = db.file_info().unwrap().page_size as u64;
    assert_eq!(options.cache_size, 1 << 20);
    assert_eq!(options.checkpoint_pages, 100);
    assert_eq!(options.checkpoint_wal_bytes, 100 * 2 * page);
    assert_eq!(options.snapshot_memory, 3 << 20);
    assert_eq!(options.checkpoint_wait, Duration::from_millis(7));
    assert_eq!(options.wait_for_readers, Duration::from_millis(9));
    assert_eq!(options.let_go_wal_bytes, Some(4 * 100 * 2 * page));
    assert_eq!(options.watch_every, Duration::from_millis(250));
    assert_eq!(options.clock_every, Duration::from_millis(1500));
}

/// WAL bytes set, and readers never asked to let go.
#[test]
fn wal_bytes_set_and_readers_never_asked() {
    let (_dir, path) = dir_and_path();
    let db = Database::open_with(
        &path,
        OpenOptions::default()
            .checkpoint_wal_bytes(12345)
            .let_go_wal_bytes(0),
    )
    .unwrap();
    let options = db.open_info().options;
    assert_eq!(options.checkpoint_wal_bytes, 12345);
    assert_eq!(options.let_go_wal_bytes, None);
}

/// Opened through a symlink: the file it leads to, beside which the WAL
/// is (SPEC §127).
#[cfg(unix)]
#[test]
fn through_a_symlink_the_file_it_leads_to() {
    let (dir, path) = dir_and_path();
    drop(Database::open(&path).unwrap());
    let link = dir.path().join("link.trunkdb");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    let db = Database::open(&link).unwrap();
    assert_eq!(db.path(), path);
    assert_eq!(db.open_info().wal_path, wal(&path));
}

/// Read-only beside a writer, and after it closed: by the lock files.
#[test]
fn read_only_beside_a_writer() {
    let (_dir, path) = dir_and_path();
    let writer = Database::open(&path).unwrap();
    let reader = Database::open_with(&path, OpenOptions::default().read_only(true)).unwrap();
    assert!(reader.is_read_only());
    assert_eq!(reader.path(), path);
    assert_eq!(reader.open_info().mode, OpenMode::ReadOnlyBesideWriter);
    drop(writer);
    let later = Database::open_with(&path, OpenOptions::default().read_only(true)).unwrap();
    assert_eq!(later.open_info().mode, OpenMode::ReadOnlyBesideWriter);
}

/// Read-only alone: in a directory it may not write, where no writer
/// made the lock files (Unix, not as root).
#[cfg(unix)]
#[test]
fn read_only_alone() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, path) = dir_and_path();
    drop(Database::open(&path).unwrap());
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let file = entry.unwrap().path();
        if file != path && file != wal(&path) {
            std::fs::remove_file(file).unwrap();
        }
    }
    let set =
        |mode| std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode)).unwrap();
    set(0o555);
    let root = std::fs::File::create(dir.path().join("probe")).is_ok();
    let reader = Database::open_with(&path, OpenOptions::default().read_only(true));
    set(0o755);
    if root {
        return;
    }
    let reader = reader.unwrap();
    assert!(reader.is_read_only());
    assert_eq!(reader.open_info().mode, OpenMode::ReadOnlyAlone);
}

/// As a document, for the C ABI and Python: names, the mode's name,
/// durations in seconds.
#[test]
fn as_a_document() {
    let (_dir, path) = dir_and_path();
    let db = Database::open_with(
        &path,
        OpenOptions::default().watch_every(Duration::from_millis(250)),
    )
    .unwrap();
    let doc = db.open_info().to_document();
    let text = |name: &str| {
        doc.get(name)
            .and_then(Document::as_str)
            .unwrap()
            .to_string()
    };
    assert_eq!(text("path"), path.to_string_lossy());
    assert_eq!(text("wal_path"), wal(&path).to_string_lossy());
    assert_eq!(text("mode"), "writer");
    assert_eq!(doc.get("read_only"), Some(&Document::Bool(false)));
    assert_eq!(doc.get("poisoned"), Some(&Document::Bool(false)));
    let options = doc.get("options").unwrap();
    assert_eq!(options.get("watch_every"), Some(&Document::Float(0.25)));
    assert_eq!(options.get("clock_every"), Some(&Document::Float(1.0)));
    assert!(matches!(options.get("cache_size"), Some(Document::Int(n)) if *n > 0));
    let reader = Database::open_with(&path, OpenOptions::default().read_only(true)).unwrap();
    let doc = reader.open_info().to_document();
    assert_eq!(
        doc.get("mode").and_then(Document::as_str),
        Some("read_only_beside_writer")
    );
    assert_eq!(doc.get("read_only"), Some(&Document::Bool(true)));
}
