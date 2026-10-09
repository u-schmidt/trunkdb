//! The `trunkdb` command (SPEC §39): look into a database file, check it,
//! compact it, export it, import into it.
//!
//! Arguments are parsed by hand: five subcommands don't need a parser
//! crate, and a library's dependencies are compiled for everyone who uses
//! the library.

use std::io::{self, IsTerminal};
use std::path::Path;
use std::process::ExitCode;
use trunkdb::Database;

const USAGE: &str = "\
usage: trunkdb <command> <file> [argument]

commands:
  info   <file>               format, pages, and every collection with its
                              document count and indexes
  check  <file>               read the whole file and check that it's
                              consistent; exit code 1 if it isn't
  compact <file>              rebuild the file into as few pages as it
                              needs, and cut it to that
  export <file> [out.jsonl]   write the database as JSON Lines, to the
                              given file or to standard output
  import <file> <in.jsonl>    read an export into the database, created if
                              it doesn't exist; `-` reads standard input
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args[..] {
        ["info", file] => existing(file).and_then(|db| info(file, &db)),
        ["check", file] => existing(file).and_then(|db| check(&db)),
        ["compact", file] => existing(file).and_then(|db| compact(&db)),
        ["export", file] => existing(file).and_then(|db| export(&db, file, None)),
        ["export", file, out] => existing(file).and_then(|db| export(&db, file, Some(out))),
        ["import", file, input] => open(file).and_then(|db| import(&db, input)),
        ["help" | "-h" | "--help"] => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(code) => code,
        Err(message) => {
            eprintln!("trunkdb: {message}");
            ExitCode::FAILURE
        }
    }
}

type Outcome = Result<ExitCode, String>;

/// `1 document`, `2 documents`.
fn count(n: usize, noun: &str) -> String {
    match n {
        1 => format!("1 {noun}"),
        n => format!("{n} {noun}s"),
    }
}

/// Opens a database that must already exist — `Database::open` would
/// create a missing one, and a typo shouldn't leave an empty database
/// behind.
fn existing(file: &str) -> Result<Database, String> {
    if !Path::new(file).is_file() {
        return Err(format!("{file}: no such file"));
    }
    open(file)
}

fn open(file: &str) -> Result<Database, String> {
    Database::open(file).map_err(|e| match e {
        trunkdb::Error::Io(e) if e.kind() == io::ErrorKind::WouldBlock => {
            format!("{file}: in use by another program (it's locked while open)")
        }
        e => format!("{file}: {e}"),
    })
}

fn info(file: &str, db: &Database) -> Outcome {
    let failed = |e: trunkdb::Error| e.to_string();
    let file_info = db.file_info().map_err(failed)?;
    println!("{file}");
    println!(
        "  format {}, {} pages of {} bytes, {} free",
        file_info.format_version, file_info.pages, file_info.page_size, file_info.free_pages
    );
    let collections = db.collections().map_err(failed)?;
    if collections.is_empty() {
        println!("  no collections");
    }
    for name in collections {
        let collection = db.collection::<trunkdb::Document>(&name);
        let documents = collection
            .count(trunkdb::query::Filter::new())
            .map_err(failed)?;
        let indexes: Vec<String> = collection
            .indexes()
            .map_err(failed)?
            .into_iter()
            .map(|index| {
                let name = index.name();
                match (index.is_unique(), index.is_sparse()) {
                    (false, false) => name,
                    (true, false) => format!("{name} (unique)"),
                    (false, true) => format!("{name} (sparse)"),
                    (true, true) => format!("{name} (unique, sparse)"),
                }
            })
            .collect();
        let documents = count(documents, "document");
        match indexes.is_empty() {
            true => println!("  {name}: {documents}"),
            false => println!("  {name}: {documents}, indexes: {}", indexes.join(", ")),
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn check(db: &Database) -> Outcome {
    let report = db.check().map_err(|e| e.to_string())?;
    for problem in &report.problems {
        println!("problem: {problem}");
    }
    let summary = format!(
        "{}, {}, {} pages",
        count(report.collections, "collection"),
        count(report.documents, "document"),
        report.pages
    );
    if report.is_ok() {
        println!("ok: {summary}");
        Ok(ExitCode::SUCCESS)
    } else {
        println!("{}: {summary}", count(report.problems.len(), "problem"));
        Ok(ExitCode::FAILURE)
    }
}

fn compact(db: &Database) -> Outcome {
    let compacted = db.compact().map_err(|e| e.to_string())?;
    let (before, after) = (compacted.pages_before, compacted.pages_after);
    if after == before {
        println!("already compact: {}", count(after as usize, "page"));
    } else {
        let page_size = db.file_info().map_err(|e| e.to_string())?.page_size;
        let freed = (before - after) * page_size as u64;
        println!(
            "compacted: {before} → {after} pages, {} smaller",
            size(freed)
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// `8 KB`, `4.3 MB`.
fn size(bytes: u64) -> String {
    match bytes {
        b if b < 1 << 20 => format!("{} KB", b >> 10),
        b if b < 1 << 30 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
    }
}

/// Whether two paths name the same file: the same inode (so hard links and
/// symlinks count), or the same resolved path where there are no inodes.
fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(a), Ok(b)) = (std::fs::metadata(a), std::fs::metadata(b)) {
            return a.dev() == b.dev() && a.ino() == b.ino();
        }
    }
    matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

/// Exports to `out` through a temporary file beside it, renamed over `out`
/// only once the export is complete: a failed export leaves an existing
/// `out` alone, and `out` can never be the database itself.
fn export_to_file(db: &Database, file: &str, out: &str) -> Result<trunkdb::Summary, String> {
    let mut wal = std::ffi::OsString::from(file);
    wal.push(".wal");
    for protected in [Path::new(file), Path::new(&wal)] {
        if same_file(Path::new(out), protected) {
            return Err(format!(
                "{out}: is the database or its log; choose another file"
            ));
        }
    }
    // Write through a symlink to its target, not over the link.
    let target = std::fs::canonicalize(out).unwrap_or_else(|_| out.into());
    let mut temp = target.clone().into_os_string();
    temp.push(format!(".tmp{}", std::process::id()));
    let temp = std::path::PathBuf::from(temp);
    let written = (|| {
        let file = std::fs::File::create(&temp)?;
        let summary = db.export(&file).map_err(io::Error::other)?;
        file.sync_all()?;
        std::fs::rename(&temp, &target)?;
        Ok::<_, io::Error>(summary)
    })();
    written.map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        format!("{out}: {e}")
    })
}

fn export(db: &Database, file: &str, out: Option<&str>) -> Outcome {
    let summary = match out {
        Some(path) => export_to_file(db, file, path),
        None => db.export(io::stdout().lock()).map_err(|e| e.to_string()),
    }?;
    // To stderr, so standard output stays pure JSON Lines.
    eprintln!(
        "exported {}, {}",
        count(summary.collections, "collection"),
        count(summary.documents, "document")
    );
    Ok(ExitCode::SUCCESS)
}

fn import(db: &Database, input: &str) -> Outcome {
    let summary = if input == "-" {
        if io::stdin().is_terminal() {
            return Err("`-` reads an export from standard input; pipe one in".to_string());
        }
        db.import(io::stdin().lock())
    } else {
        let file = std::fs::File::open(input).map_err(|e| format!("{input}: {e}"))?;
        db.import(io::BufReader::new(file))
    }
    .map_err(|e| e.to_string())?;
    eprintln!(
        "imported {}, {}",
        count(summary.collections, "collection"),
        count(summary.documents, "document")
    );
    Ok(ExitCode::SUCCESS)
}
