//! The `trunkdb` command (SPEC §39): look into a database file, check it,
//! compact it, export it, import into it, find documents in it (§112).
//! Looking, checking, exporting and finding open it read-only (SPEC §88).
//!
//! Arguments are parsed by hand: six subcommands don't need a parser
//! crate, and a library's dependencies are compiled for everyone who uses
//! the library.

use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;
use trunkdb::query::{Filter, ParseFilterError};
use trunkdb::{Database, Document, OpenOptions};

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
  import <file> <in.jsonl> [--max-line <bytes|none>]
                              read an export into the database, created if
                              it doesn't exist; `-` reads standard input.
                              A line over 256 MiB (or the given size) stops
                              the import; `none` turns the limit off
  find   <file> <collection> [filter] [--explain]
                              print the documents the filter finds, one
                              JSON document per line; every document
                              without a filter. A filter is text like
                              'age >= 30 order by name limit 10'; words
                              after the collection are joined into one.
                              --explain runs it and prints how instead:
                              the plan, the index reads, the counts

info, check, export and find only read: they change nothing in the file or its
log, not even what a crash of the program that had it open left there.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result =
        match args[..] {
            ["info", file] => read_only(file).and_then(|db| info(file, &db)),
            ["check", file] => read_only(file).and_then(|db| check(&db)),
            ["compact", file] => existing(file).and_then(|db| compact(&db)),
            ["export", file] => read_only(file).and_then(|db| export(&db, file, None)),
            ["export", file, out] => read_only(file).and_then(|db| export(&db, file, Some(out))),
            ["import", file, input] => open(file).and_then(|db| import(&db, input, None)),
            ["find", file, collection, ref filter @ .., "--explain"] => read_only(file)
                .and_then(|db| find(&db, collection, &filter.join(" "), Show::Explained)),
            ["find", file, collection, ref filter @ ..] => read_only(file)
                .and_then(|db| find(&db, collection, &filter.join(" "), Show::Documents)),
            ["import", file, input, "--max-line", limit] => max_line(limit)
                .and_then(|limit| open(file).and_then(|db| import(&db, input, Some(limit)))),
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

/// `existing`, opened read-only (SPEC §88): to look at, writing nothing.
fn read_only(file: &str) -> Result<Database, String> {
    if !Path::new(file).is_file() {
        return Err(format!("{file}: no such file"));
    }
    open_with(file, OpenOptions::default().read_only(true))
}

fn open(file: &str) -> Result<Database, String> {
    open_with(file, OpenOptions::default())
}

fn open_with(file: &str, options: OpenOptions) -> Result<Database, String> {
    Database::open_with(file, options).map_err(|e| match e {
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
    // The sizes (SPEC §93) after the lines that were there before, which
    // scripts may read; on a file they can't be read from, those still are.
    let stats = match db.stats() {
        Ok(stats) => {
            print_page_use(&stats);
            Some(stats)
        }
        Err(e) => {
            println!("  sizes not available: {e} (`trunkdb check` says more)");
            None
        }
    };
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
        let sizes = stats
            .as_ref()
            .and_then(|stats| stats.collections.iter().find(|c| c.name == name));
        if let Some(sizes) = sizes {
            print_collection_sizes(sizes);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `info`'s summary of where the pages go (SPEC §93).
fn print_page_use(stats: &trunkdb::Stats) {
    let pages = &stats.pages;
    let mut parts = vec![
        format!("{} header", pages.header),
        format!("{} catalog", pages.catalog),
        format!("{} data", pages.data),
        format!("{} overflow", pages.overflow),
        format!("{} index", pages.index),
        format!("{} free", pages.free),
    ];
    if pages.unaccounted > 0 {
        parts.push(format!("{} unaccounted", pages.unaccounted));
    }
    println!("  pages: {}", parts.join(", "));
    let after = stats.compacted_pages_estimate();
    match stats.file.pages - after {
        0 => println!("  compact would not make it smaller"),
        saved => println!(
            "  compact would leave {}, {} fewer",
            count(after as usize, "page"),
            bytes(saved * stats.file.page_size as u64)
        ),
    }
    if let Some(wal) = stats.wal_bytes {
        println!("  WAL: {}", bytes(wal));
    }
    if let Some(ref recovered) = stats.recovered {
        println!(
            "  recovered {} from the WAL{}",
            count(recovered.pages as usize, "page"),
            if recovered.in_memory {
                ", in memory"
            } else {
                ""
            }
        );
    }
}

/// One collection's sizes, under its line.
fn print_collection_sizes(sizes: &trunkdb::CollectionStats) {
    let index_pages: u64 = std::iter::once(&sizes.primary)
        .chain(&sizes.indexes)
        .map(|index| index.pages)
        .sum();
    let overflow = match sizes.overflow_pages {
        0 => String::new(),
        n => format!(" and {}", count(n as usize, "overflow page")),
    };
    println!(
        "    {} of documents in {}{overflow}, {} free in them; indexes {}",
        bytes(sizes.document_bytes),
        count(sizes.data_pages as usize, "data page"),
        bytes(sizes.free_bytes),
        count(index_pages as usize, "page"),
    );
}

/// `512 bytes`, `4.0 KiB`, `1.2 MiB`, `3.4 GiB`.
fn bytes(n: u64) -> String {
    const UNITS: [&str; 3] = ["KiB", "MiB", "GiB"];
    if n < 1024 {
        return count(n as usize, "byte");
    }
    let mut value = n as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
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

/// The argument of `--max-line`: a size in bytes, or `none`.
fn max_line(text: &str) -> Result<usize, String> {
    match text {
        "none" => Ok(usize::MAX),
        _ => text
            .parse()
            .map_err(|_| format!("--max-line: {text:?} is not a number of bytes or `none`")),
    }
}

fn import(db: &Database, input: &str, max_line: Option<usize>) -> Outcome {
    let options = match max_line {
        Some(bytes) => trunkdb::ImportOptions::default().max_line_bytes(bytes),
        None => trunkdb::ImportOptions::default(),
    };
    let summary = if input == "-" {
        if io::stdin().is_terminal() {
            return Err("`-` reads an export from standard input; pipe one in".to_string());
        }
        db.import_with(io::stdin().lock(), options)
    } else {
        let file = std::fs::File::open(input).map_err(|e| format!("{input}: {e}"))?;
        db.import_with(io::BufReader::new(file), options)
    }
    .map_err(|e| e.to_string())?;
    eprintln!(
        "imported {}, {}",
        count(summary.collections, "collection"),
        count(summary.documents, "document")
    );
    Ok(ExitCode::SUCCESS)
}

/// What `find` prints.
#[derive(Clone, Copy)]
enum Show {
    /// The documents found, one JSON line each (SPEC §112).
    Documents,
    /// How the query ran, `--explain` (SPEC §113).
    Explained,
}

/// The documents `filter` finds in `collection`, one JSON line each
/// (SPEC §112), streamed from a cursor; how many, to stderr. Or how it
/// found them (§113).
fn find(db: &Database, collection: &str, filter: &str, show: Show) -> Outcome {
    let failed = |e: trunkdb::Error| e.to_string();
    let filter: Filter = match filter.parse() {
        Ok(filter) => filter,
        Err(e) => {
            eprintln!("trunkdb: {}", filter_error(filter, &e));
            return Ok(ExitCode::from(2));
        }
    };
    // A typo in the name would find nothing, as if nothing matched.
    if !db
        .collections()
        .map_err(failed)?
        .iter()
        .any(|c| c == collection)
    {
        return Err(format!("{collection}: no such collection"));
    }
    let docs = db.collection::<Document>(collection);
    if let Show::Explained = show {
        let explained = docs.explain_run(filter).map_err(failed)?;
        return match io::stdout()
            .lock()
            .write_all(explanation(&explained).as_bytes())
        {
            Ok(()) => Ok(ExitCode::SUCCESS),
            Err(e) => stopped_writing(e),
        };
    }
    let mut out = io::BufWriter::new(io::stdout().lock());
    let mut found = 0;
    for doc in docs.cursor(filter).map_err(failed)? {
        let doc = doc.map_err(failed)?;
        if let Err(e) = writeln!(out, "{doc}") {
            return stopped_writing(e);
        }
        found += 1;
    }
    if let Err(e) = out.flush() {
        return stopped_writing(e);
    }
    eprintln!("found {}", count(found, "document"));
    Ok(ExitCode::SUCCESS)
}

/// A write to standard output that failed: the reader gone, as `| head`
/// goes once it has its lines, is the end of what anyone wants.
fn stopped_writing(e: io::Error) -> Outcome {
    match e.kind() {
        io::ErrorKind::BrokenPipe => Ok(ExitCode::SUCCESS),
        _ => Err(format!("standard output: {e}")),
    }
}

/// The filter's mistake, with its line of the filter and a `^` under
/// where it is.
fn filter_error(filter: &str, e: &ParseFilterError) -> String {
    let at = e.offset();
    let start = filter[..at].rfind('\n').map_or(0, |i| i + 1);
    let end = filter[at..].find('\n').map_or(filter.len(), |i| at + i);
    let column = filter[start..at].chars().count();
    format!(
        "filter: {e}\n  {}\n  {}^",
        &filter[start..end],
        " ".repeat(column)
    )
}

/// How a query ran (SPEC §113), a line for each thing `explain_run` says.
fn explanation(explained: &trunkdb::query::Explained) -> String {
    use std::fmt::Write;
    use trunkdb::query::QueryPlan;
    let mut text = String::new();
    let plan = match &explained.plan {
        QueryPlan::Scan => "scan of every document".to_string(),
        QueryPlan::Index { field, .. } => format!("index on {field}"),
        QueryPlan::IndexOrder { field, .. } => format!("index on {field}, in its order"),
        QueryPlan::IndexUnion { fields, .. } => {
            format!("indexes on {}, together", fields.join(", "))
        }
        QueryPlan::ById => "by id".to_string(),
        other => format!("{other:?}"),
    };
    let _ = writeln!(text, "plan: {plan}");
    for read in &explained.reads {
        let bounds = match read.bounds.as_slice() {
            [] => "the whole index".to_string(),
            bounds => {
                let bounds: Vec<String> = bounds.iter().map(|b| b.to_string()).collect();
                format!("where {}", bounds.join(" and "))
            }
        };
        let _ = write!(
            text,
            "read: {}, {bounds}, {}",
            read.index,
            match read.entries {
                1 => "1 entry".to_string(),
                n => format!("{n} entries"),
            }
        );
        if read.backward {
            text.push_str(", backward");
        }
        if read.covering {
            text.push_str(", its entries decide the filter");
        }
        text.push('\n');
    }
    if let Some(ref sort) = explained.sort {
        let _ = writeln!(
            text,
            "sort: {} from the index order, {} in memory",
            count(sort.from_index, "key"),
            sort.in_memory
        );
    }
    let stats = &explained.stats;
    let _ = writeln!(text, "keys examined: {}", stats.keys_examined);
    let _ = writeln!(text, "documents examined: {}", stats.documents_examined);
    let _ = writeln!(text, "documents matched: {}", stats.documents_matched);
    let _ = writeln!(text, "documents returned: {}", stats.documents_returned);
    let _ = writeln!(text, "answered from index entries: {}", stats.from_entries);
    let _ = writeln!(text, "time: {:.2} ms", stats.elapsed.as_secs_f64() * 1000.0);
    text
}
