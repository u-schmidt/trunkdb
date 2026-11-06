//! The `trunkdb` command (SPEC §39): look into a database file, check it,
//! compact it, export it, import into it, find documents in it (§112),
//! sum up the fields its documents have (§121), count them and get one by
//! id, with JSON for a program or an agent to read (§155); insert,
//! update and delete documents, add and drop indexes, drop a collection
//! (§156), and write many ops as one batch, one op a line (§168).
//! Looking, checking, exporting, finding, summing up, counting and
//! getting open it read-only (SPEC §88). A filter's or an
//! aggregate's text takes parameters, their values given apart with
//! `--param` (§163).
//!
//! Arguments are parsed by hand: seven subcommands don't need a parser
//! crate, and a library's dependencies are compiled for everyone who uses
//! the library.

use std::cell::RefCell;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;
use std::str::FromStr;
use trunkdb::query::{BindError, Filter, Params, ParseFilterError, Template};
use trunkdb::{Database, Document, OpenOptions};

const USAGE: &str = "\
usage: trunkdb <command> <file> [argument]

commands:
  info   <file> [--json]      format, pages, and every collection with its
                              document count and indexes
  check  <file> [--json]      read the whole file and check that it's
                              consistent; exit code 1 if it isn't
  compact <file>              rebuild the file into as few pages as it
                              needs, and cut it to that
  export <file> [out.jsonl]   write the database as JSON Lines, to the
                              given file or to standard output
  verify <in.jsonl>           whether an export restores: restored into a
                              temporary file, checked and counted, then
                              deleted; `-` reads standard input. Exit
                              code 1, and why, if it doesn't
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
  fields <file> <collection> [filter] [--json]
                              every path the documents have, in how many
                              of them, and as which types: one line each,
                              to paste into a filter. A filter sums up
                              only what it finds; `limit 1000` a sample
  count  <file> <collection> [filter]
                              how many documents the filter finds, every
                              document's count without one; from index
                              entries where they can tell, reading none
  get    <file> <collection> <id>
                              the document with this id, as one JSON line;
                              exit code 1 if there is none
  aggregate <file> <collection> <filter> <aggregate> [--explain]
                              groups of the documents the filter finds,
                              one JSON row per line, the aggregate as
                              'count() as n, avg(hours) as mean group by
                              team having n > 5 order by n desc limit 10';
                              columns count() (of documents, no path)
                              sum() min() max() avg() count(distinct p)
                              first(p) last(p), first and last with
                              `order by p` inside, each with `filter
                              (where …)` if it takes in only some
                              documents; `unwind items` and `window at
                              5m` before them. --explain says how

--json prints what a program reads: info and check one JSON object, fields
one JSON object per path.

writing (each command is one atomic batch; the file must exist but for insert --create):
  insert <file> <collection> <document | -> [--create]
                              one document as JSON, or `-`: one per line on
                              standard input; prints each new id. A missing
                              collection is an error; --create makes it
  update <file> <collection> <filter> <update> [--all] [--upsert] [--dry-run]
                              change the documents the filter finds, the
                              update as JSON operators, like
                              '{\"$set\": {\"status\": \"done\"}, \"$inc\": {\"n\": 1}}':
                              $set $setOnInsert $unset $inc $mul $min $max
                              $rename $push ($each) $addToSet ($each) $pull
                              $pop (1 last, -1 first). --upsert inserts a
                              document made of the filter's `=`s where none
                              matches
  delete <file> <collection> <filter> [--all] [--dry-run]
                              delete the documents the filter finds
  batch  <file> <ops.jsonl | -> [--create] [--dry-run]
                              one op a line as JSON, all committed as one
                              batch or none of them, each at its place
                              seeing what the ones before wrote:
                              {\"op\": \"insert\", \"collection\": \"log\", \"document\": {…}}
                              {\"op\": \"update\", \"collection\": \"tasks\",
                               \"filter\": \"due < :now\", \"update\": {\"$set\": …}}
                              {\"op\": \"delete\", \"collection\": …, \"filter\": …}
                              {\"op\": \"require\" or \"require_none\", \"collection\": …,
                               \"filter\": …}: must find a document, or none,
                              or nothing is written. An update or delete
                              with an empty filter needs \"all\": true.
                              Prints each new id; a failure names its
                              line. --create makes missing collections;
                              --dry-run says what it would do
  index  <file> <collection> add <field>... [--unique] [--sparse]
  index  <file> <collection> drop <field>...
                              an index on one field, or on several in order
  drop   <file> <collection>  the collection, its documents and indexes

update and delete refuse to change every document of a collection unless
given --all; --dry-run says how many they would change, writing nothing.
The filter is one argument: quote it, '' for none.

parameters (find, fields, count, aggregate, update, delete, batch):
  --param <name>=<value>      a value for `:name` in the filter or the
                              aggregate, where a value or a count stands:
                              'status = :s limit :n' --param s=open
                              --param n=20. The value is never read as
                              text, so it can't change what the filter
                              says. It is JSON (5, true, null, \"text\",
                              [1, 2], {\"a\": 1}), id(\"…\") or date(\"…\");
                              anything else is the string as written. A
                              string that would read as one of those is
                              given as JSON: n='\"20\"' is a string

info, check, export, find, fields, count, get and aggregate only read: they change
nothing in the file or its log, not even what a crash of the program that
had it open left there.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (args, given) = match given_params(&args) {
        Ok(split) => split,
        Err(why) => {
            eprintln!("trunkdb: {why}");
            return ExitCode::from(2);
        }
    };
    let takes_params = [
        "find",
        "fields",
        "count",
        "aggregate",
        "update",
        "delete",
        "batch",
    ];
    if !given.values.is_empty() && !args.first().is_some_and(|c| takes_params.contains(c)) {
        eprintln!("trunkdb: --param: not an option of this command");
        return ExitCode::from(2);
    }
    let given = &given;
    let result = match args[..] {
        ["info", file] => read_only(file).and_then(|db| info(file, &db)),
        ["info", file, "--json"] => read_only(file).and_then(|db| info_json(file, &db)),
        ["check", file] => read_only(file).and_then(|db| check(&db, Format::Text)),
        ["check", file, "--json"] => read_only(file).and_then(|db| check(&db, Format::Json)),
        ["compact", file] => existing(file).and_then(|db| compact(&db)),
        ["export", file] => read_only(file).and_then(|db| export(&db, file, None)),
        ["export", file, out] => read_only(file).and_then(|db| export(&db, file, Some(out))),
        ["import", file, input] => open(file).and_then(|db| import(&db, input, None)),
        ["verify", input] => verify(input),
        ["find", file, collection, ref filter @ .., "--explain"] => read_only(file)
            .and_then(|db| find(&db, collection, &filter.join(" "), given, Show::Explained)),
        ["find", file, collection, ref filter @ ..] => read_only(file)
            .and_then(|db| find(&db, collection, &filter.join(" "), given, Show::Documents)),
        ["fields", file, collection, ref filter @ .., "--json"] => read_only(file)
            .and_then(|db| fields(&db, collection, &filter.join(" "), given, Format::Json)),
        ["fields", file, collection, ref filter @ ..] => read_only(file)
            .and_then(|db| fields(&db, collection, &filter.join(" "), given, Format::Text)),
        ["count", file, collection, ref filter @ ..] => {
            read_only(file).and_then(|db| count_of(&db, collection, &filter.join(" "), given))
        }
        ["get", file, collection, id] => read_only(file).and_then(|db| get(&db, collection, id)),
        ["aggregate", file, collection, filter, text] => read_only(file)
            .and_then(|db| aggregate(&db, collection, filter, text, given, Show::Documents)),
        ["aggregate", file, collection, filter, text, "--explain"] => read_only(file)
            .and_then(|db| aggregate(&db, collection, filter, text, given, Show::Explained)),
        ["insert", file, collection, ref rest @ ..] => insert(file, collection, rest),
        ["update", file, collection, ref rest @ ..] => update(file, collection, rest, given),
        ["delete", file, collection, ref rest @ ..] => delete(file, collection, rest, given),
        ["batch", file, ref rest @ ..] => batch(file, rest, given),
        ["index", file, collection, "add", ref rest @ ..] => index_add(file, collection, rest),
        ["index", file, collection, "drop", ref rest @ ..] => index_drop(file, collection, rest),
        ["drop", file, collection] => drop_collection(file, collection),
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

/// `trunkdb info`: what `info_text` gathers, written at once, so that a
/// reader that goes, as `| head` does, ends it quietly, as for `find`
/// (SPEC §112). What was gathered before a failure is still printed, as
/// it was when every line went out on its own.
fn info(file: &str, db: &Database) -> Outcome {
    let mut text = String::new();
    let gathered = info_text(file, db, &mut text);
    if let Err(e) = io::stdout().lock().write_all(text.as_bytes()) {
        stopped_writing(e)?;
    }
    gathered.map(|()| ExitCode::SUCCESS)
}

/// `trunkdb info --json` (SPEC §155): what `info` prints, as one JSON
/// object, every count a number and every size in bytes.
fn info_json(file: &str, db: &Database) -> Outcome {
    let failed = |e: trunkdb::Error| e.to_string();
    let file_info = db.file_info().map_err(failed)?;
    let release = |release: Option<trunkdb::Release>| release.map(|r| r.to_string());
    let stats = db.stats().ok();
    let mut collections = Vec::new();
    for name in db.collections().map_err(failed)? {
        let collection = db.collection::<Document>(&name);
        let documents = collection.count(Filter::new()).map_err(failed)?;
        let indexes: Vec<serde_json::Value> = (collection.indexes().map_err(failed)?.iter())
            .map(|index| {
                serde_json::json!({
                    "fields": index.fields(),
                    "unique": index.is_unique(),
                    "sparse": index.is_sparse(),
                })
            })
            .collect();
        let sizes = (stats.as_ref())
            .and_then(|stats| stats.collections.iter().find(|c| c.name == name))
            .map(|sizes| {
                let index_pages: u64 = (std::iter::once(&sizes.primary).chain(&sizes.indexes))
                    .map(|index| index.pages)
                    .sum();
                serde_json::json!({
                    "document_bytes": sizes.document_bytes,
                    "data_pages": sizes.data_pages,
                    "overflow_pages": sizes.overflow_pages,
                    "free_bytes": sizes.free_bytes,
                    "index_pages": index_pages,
                })
            });
        collections.push(serde_json::json!({
            "name": name,
            "documents": documents,
            "indexes": indexes,
            "sizes": sizes,
        }));
    }
    print_json(&serde_json::json!({
        "file": file,
        "format": file_info.format_version,
        "page_size": file_info.page_size,
        "pages": file_info.pages,
        "free_pages": file_info.free_pages,
        "written_by": release(file_info.written_by),
        "created_by": release(file_info.created_by),
        "wal_bytes": stats.as_ref().and_then(|stats| stats.wal_bytes),
        "collections": collections,
    }))
}

/// The lines `info` prints, into `text`.
fn info_text(file: &str, db: &Database, text: &mut String) -> Result<(), String> {
    use std::fmt::Write;
    let failed = |e: trunkdb::Error| e.to_string();
    let file_info = db.file_info().map_err(failed)?;
    let _ = writeln!(text, "{file}");
    let _ = writeln!(
        text,
        "  format {}, {} pages of {} bytes, {} free",
        file_info.format_version, file_info.pages, file_info.page_size, file_info.free_pages
    );
    // The releases (SPEC §125.4), with the file's other facts; the
    // lines that were there before keep their words.
    let release = |release: Option<trunkdb::Release>| {
        release.map_or_else(|| "unknown".to_string(), |release| release.to_string())
    };
    let _ = writeln!(
        text,
        "  written by {}, created by {}",
        release(file_info.written_by),
        release(file_info.created_by)
    );
    // The sizes (SPEC §93) after the lines that were there before, which
    // scripts may read; on a file they can't be read from, those still are.
    let stats = match db.stats() {
        Ok(stats) => {
            print_page_use(&stats, text);
            Some(stats)
        }
        Err(e) => {
            let _ = writeln!(
                text,
                "  sizes not available: {e} (`trunkdb check` says more)"
            );
            None
        }
    };
    let collections = db.collections().map_err(failed)?;
    if collections.is_empty() {
        let _ = writeln!(text, "  no collections");
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
        let _ = match indexes.is_empty() {
            true => writeln!(text, "  {name}: {documents}"),
            false => writeln!(
                text,
                "  {name}: {documents}, indexes: {}",
                indexes.join(", ")
            ),
        };
        let sizes = stats
            .as_ref()
            .and_then(|stats| stats.collections.iter().find(|c| c.name == name));
        if let Some(sizes) = sizes {
            print_collection_sizes(sizes, text);
        }
    }
    Ok(())
}

/// `info`'s summary of where the pages go (SPEC §93).
fn print_page_use(stats: &trunkdb::Stats, text: &mut String) {
    use std::fmt::Write;
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
    let _ = writeln!(text, "  pages: {}", parts.join(", "));
    let after = stats.compacted_pages_estimate();
    let _ = match stats.file.pages - after {
        0 => writeln!(text, "  compact would not make it smaller"),
        saved => writeln!(
            text,
            "  compact would leave {}, {} fewer",
            count(after as usize, "page"),
            bytes(saved * stats.file.page_size as u64)
        ),
    };
    if let Some(wal) = stats.wal_bytes {
        let _ = writeln!(text, "  WAL: {}", bytes(wal));
    }
    if let Some(ref recovered) = stats.recovered {
        let _ = writeln!(
            text,
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
fn print_collection_sizes(sizes: &trunkdb::CollectionStats, text: &mut String) {
    use std::fmt::Write;
    let index_pages: u64 = std::iter::once(&sizes.primary)
        .chain(&sizes.indexes)
        .map(|index| index.pages)
        .sum();
    let overflow = match sizes.overflow_pages {
        0 => String::new(),
        n => format!(" and {}", count(n as usize, "overflow page")),
    };
    let _ = writeln!(
        text,
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

/// How a command prints what it found: for a person, or as JSON for a
/// program (SPEC §155).
#[derive(Clone, Copy, PartialEq)]
enum Format {
    Text,
    Json,
}

/// One JSON value on its own line, to standard output.
fn print_json(value: &serde_json::Value) -> Outcome {
    match writeln!(io::stdout().lock(), "{value}") {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(e) => stopped_writing(e),
    }
}

fn check(db: &Database, format: Format) -> Outcome {
    let report = db.check().map_err(|e| e.to_string())?;
    if format == Format::Json {
        print_json(&serde_json::json!({
            "ok": report.is_ok(),
            "collections": report.collections,
            "documents": report.documents,
            "pages": report.pages,
            "problems": report.problems,
        }))?;
        return Ok(match report.is_ok() {
            true => ExitCode::SUCCESS,
            false => ExitCode::FAILURE,
        });
    }
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
    // The WAL beside the file a symlink names, as the open uses (SPEC §127).
    let resolved = std::fs::canonicalize(file).unwrap_or_else(|_| file.into());
    let mut wal = resolved.into_os_string();
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

/// `trunkdb verify` (SPEC §165.2): `input` restored into a temporary
/// file, checked, counted and deleted, by `verify_export`. What was
/// restored to standard output; exit code 1 and why, if it doesn't
/// restore whole.
fn verify(input: &str) -> Outcome {
    let at = std::env::temp_dir().join(format!("trunkdb-verify-{}.trunkdb", std::process::id()));
    let verified = if input == "-" {
        if io::stdin().is_terminal() {
            return Err("`-` reads an export from standard input; pipe one in".to_string());
        }
        trunkdb::verify_export(io::stdin().lock(), &at)
    } else {
        let file = std::fs::File::open(input).map_err(|e| format!("{input}: {e}"))?;
        trunkdb::verify_export(io::BufReader::new(file), &at)
    };
    let verified = match verified {
        Ok(verified) => verified,
        Err(trunkdb::Error::Import { line, message, .. }) => {
            eprintln!("trunkdb: {input}: doesn't restore: line {line}: {message}");
            return Ok(ExitCode::FAILURE);
        }
        Err(e) => return Err(format!("{input}: {e}")),
    };
    if !verified.is_ok() {
        eprintln!("trunkdb: {input}: doesn't restore whole:");
        for problem in &verified.problems {
            eprintln!("  {problem}");
        }
        return Ok(ExitCode::FAILURE);
    }
    let indexes = match verified.indexes {
        1 => "1 index".to_string(),
        n => format!("{n} indexes"),
    };
    println!(
        "verified {input}: {}, {}, {indexes}",
        count(verified.collections, "collection"),
        count(verified.documents, "document"),
    );
    if !verified.ended {
        eprintln!(
            "trunkdb: {input} is an export of version 1 or 2, without an end line: \
             it restores, but whether it is whole can't be told"
        );
    }
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
fn find(db: &Database, collection: &str, filter: &str, given: &Given, show: Show) -> Outcome {
    let failed = |e: trunkdb::Error| e.to_string();
    let (docs, filter) = match query(db, collection, filter, given)? {
        Ok(query) => query,
        Err(usage) => return Ok(usage),
    };
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

/// `trunkdb aggregate` (SPEC §157): the rows of `aggregate` over what
/// `filter` finds, one JSON line each, how many to stderr; or how it ran.
fn aggregate(
    db: &Database,
    collection: &str,
    filter: &str,
    text: &str,
    given: &Given,
    show: Show,
) -> Outcome {
    let failed = |e: trunkdb::Error| e.to_string();
    let parsed = match bound(
        "aggregate",
        text,
        given,
        Template::<trunkdb::query::Aggregate>::bind,
    ) {
        Ok(aggregate) => aggregate,
        Err(usage) => return Ok(usage),
    };
    let (docs, filter) = match query(db, collection, filter, given)? {
        Ok(query) => query,
        Err(usage) => return Ok(usage),
    };
    if let Show::Explained = show {
        let explained = docs.explain_aggregate(filter, &parsed).map_err(failed)?;
        return match io::stdout()
            .lock()
            .write_all(explanation(&explained).as_bytes())
        {
            Ok(()) => Ok(ExitCode::SUCCESS),
            Err(e) => stopped_writing(e),
        };
    }
    let rows = docs.aggregate(filter, &parsed).map_err(failed)?;
    let mut out = io::BufWriter::new(io::stdout().lock());
    for row in &rows {
        if let Err(e) = writeln!(out, "{row}") {
            return stopped_writing(e);
        }
    }
    if let Err(e) = out.flush() {
        return stopped_writing(e);
    }
    eprintln!("{}", count(rows.len(), "row"));
    Ok(ExitCode::SUCCESS)
}

/// `trunkdb count` (SPEC §155): how many documents the filter finds, the
/// number alone on standard output, its skip and limit applied as `find`
/// applies them.
fn count_of(db: &Database, collection: &str, filter: &str, given: &Given) -> Outcome {
    let (docs, filter) = match query(db, collection, filter, given)? {
        Ok(query) => query,
        Err(usage) => return Ok(usage),
    };
    let n = docs.count(filter).map_err(|e| e.to_string())?;
    match writeln!(io::stdout().lock(), "{n}") {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(e) => stopped_writing(e),
    }
}

/// `trunkdb get` (SPEC §155): the document with this id, one JSON line,
/// as `find` prints it; exit code 1, and why on stderr, if there is none.
fn get(db: &Database, collection: &str, id: &str) -> Outcome {
    // Bare, as `find` prints it inside `{"$id": …}`, or as a filter
    // writes it, `id("…")`.
    let bare = id
        .strip_prefix("id(\"")
        .and_then(|rest| rest.strip_suffix("\")"))
        .unwrap_or(id);
    let Ok(parsed) = bare.parse::<trunkdb::DocId>() else {
        eprintln!(
            "trunkdb: {id:?} is not an id: one is written as 0192f0c4-7d7a-7c43-9e0b-1f6d1b8f0a11"
        );
        return Ok(ExitCode::from(2));
    };
    let (docs, _filter) = match query(db, collection, "", &Given::default())? {
        Ok(query) => query,
        Err(usage) => return Ok(usage),
    };
    match docs.get(&parsed).map_err(|e| e.to_string())? {
        Some(doc) => match writeln!(io::stdout().lock(), "{doc}") {
            Ok(()) => Ok(ExitCode::SUCCESS),
            Err(e) => stopped_writing(e),
        },
        None => {
            eprintln!("no document {bare} in {collection}");
            Ok(ExitCode::FAILURE)
        }
    }
}

/// The values of `--param name=value` (SPEC §163.6), and the names a
/// text of the command has used.
#[derive(Default)]
struct Given {
    values: Vec<(String, Document)>,
    used: RefCell<Vec<String>>,
}

impl Given {
    /// A value no text took, the first given.
    fn unused(&self) -> Option<String> {
        let used = self.used.borrow();
        (self.values.iter())
            .map(|(name, _)| name)
            .find(|name| !used.contains(name))
            .cloned()
    }
}

/// `args` without each `--param name=value`, and those values. A
/// `--param` without `=`, or a name given twice, is the error.
fn given_params<'a>(args: &[&'a str]) -> Result<(Vec<&'a str>, Given), String> {
    let mut rest = Vec::new();
    let mut given = Given::default();
    let mut args = args.iter();
    while let Some(&arg) = args.next() {
        if arg != "--param" {
            rest.push(arg);
            continue;
        }
        let Some((name, text)) = args.next().and_then(|param| param.split_once('=')) else {
            return Err("--param takes a name and a value: --param status=open".to_string());
        };
        if given.values.iter().any(|(given, _)| given == name) {
            return Err(format!("--param {name}: given twice"));
        }
        let value = param_value(text).map_err(|why| format!("--param {name}: {why}"))?;
        given.values.push((name.to_string(), value));
    }
    Ok((rest, given))
}

/// A `--param`'s value (SPEC §163.6): a value as a filter writes one,
/// JSON, `id("…")` or `date("…")`; what doesn't start as one of those is
/// a string as written. One that starts as one and doesn't read is a
/// mistake, not a string: `[1, 2` or `date("2026-13-01")`.
fn param_value(text: &str) -> Result<Document, String> {
    if let Ok(value) = text.parse::<Document>() {
        return Ok(value);
    }
    let call = |name: &str| {
        let inner = text.trim().strip_prefix(name)?.strip_prefix('(')?;
        let inner = inner.strip_suffix(')')?;
        Some(serde_json::from_str::<String>(inner.trim()).ok())
    };
    if let Some(inner) = call("id") {
        let id = inner.and_then(|inner| inner.parse::<trunkdb::DocId>().ok());
        return id
            .map(Document::Id)
            .ok_or_else(|| format!("{text} is not an id"));
    }
    if let Some(inner) = call("date") {
        let time = inner.and_then(|inner| inner.parse::<trunkdb::DateTime>().ok());
        return (time.map(Document::DateTime)).ok_or_else(|| {
            format!("{text} is not a date-time like date(\"2026-11-03T12:00:00Z\")")
        });
    }
    match text.trim_start().starts_with(['[', '{', '"']) {
        true => Err(format!(
            "{text} starts as JSON and isn't: a string is given as JSON, \"…\""
        )),
        false => Ok(Document::String(text.to_string())),
    }
}

/// `text`, a `what`, read as a template and bound to the values `given`
/// that it has parameters for. A mistake is printed, and is exit code 2.
fn bound<T>(
    what: &str,
    text: &str,
    given: &Given,
    bind: fn(&Template<T>, &Params) -> Result<T, BindError>,
) -> Result<T, ExitCode>
where
    Template<T>: FromStr<Err = ParseFilterError>,
{
    let template: Template<T> = match text.parse() {
        Ok(template) => template,
        Err(e) => {
            eprintln!("trunkdb: {}", text_error(what, text, &e));
            return Err(ExitCode::from(2));
        }
    };
    let mut params = Params::new();
    for name in template.params() {
        if let Some((_, value)) = given.values.iter().find(|(given, _)| given == name) {
            params = params.set(name, value.clone());
            given.used.borrow_mut().push(name.to_string());
        }
    }
    bind(&template, &params).map_err(|e| {
        let message = match &e {
            BindError::Missing { name, .. } => format!("{e}: give it with --param {name}=…"),
            _ => e.to_string(),
        };
        match e.offset() {
            Some(at) => eprintln!("trunkdb: {}", marked(what, text, at, message)),
            None => eprintln!("trunkdb: {what}: {message}"),
        }
        ExitCode::from(2)
    })
}

/// A collection, and the filter to read it with.
type Query = (trunkdb::Collection<Document>, Filter);

/// `filter` read and bound to the `--param` values, and `collection`
/// checked to be there: what `find` and `fields` start with. The last
/// text of a command, so every value must have been used by now. A
/// filter that doesn't read or bind is printed with its mistake, and
/// comes back as exit code 2 in the inner `Err`.
fn query(
    db: &Database,
    collection: &str,
    filter: &str,
    given: &Given,
) -> Result<Result<Query, ExitCode>, String> {
    let parsed = match bound("filter", filter, given, Template::<Filter>::bind) {
        Ok(filter) => filter,
        Err(usage) => return Ok(Err(usage)),
    };
    if let Some(name) = given.unused() {
        eprintln!("trunkdb: --param {name}: no text of this command has the parameter :{name}");
        return Ok(Err(ExitCode::from(2)));
    }
    // A typo in the name would find nothing, as if nothing matched.
    if !db
        .collections()
        .map_err(|e| e.to_string())?
        .iter()
        .any(|c| c == collection)
    {
        return Err(format!("{collection}: no such collection"));
    }
    Ok(Ok((db.collection::<Document>(collection), parsed)))
}

/// A write to standard output that failed: the reader gone, as `| head`
/// goes once it has its lines, is the end of what anyone wants.
fn stopped_writing(e: io::Error) -> Outcome {
    match e.kind() {
        io::ErrorKind::BrokenPipe => Ok(ExitCode::SUCCESS),
        _ => Err(format!("standard output: {e}")),
    }
}

/// A text's mistake, `what` the text is (a filter, an aggregate, SPEC
/// §157), with its line of the text and a `^` under where it is.
fn text_error(what: &str, filter: &str, e: &ParseFilterError) -> String {
    marked(what, filter, e.offset(), e)
}

/// `message` about `what`, with the line of `text` it is about and a
/// `^` under byte `at`.
fn marked(what: &str, filter: &str, at: usize, message: impl std::fmt::Display) -> String {
    let e = message;
    let start = filter[..at].rfind('\n').map_or(0, |i| i + 1);
    let end = filter[at..].find('\n').map_or(filter.len(), |i| at + i);
    let column = filter[start..at].chars().count();
    format!(
        "{what}: {e}\n  {}\n  {}^",
        &filter[start..end],
        " ".repeat(column)
    )
}

/// How a query ran (SPEC §113), a line for each thing `explain_run` says.
fn explanation(explained: &trunkdb::query::Explained) -> String {
    use std::fmt::Write;
    use trunkdb::query::EntryCount;
    let mut text = String::new();
    let plan = plan_words(&explained.plan);
    let hinted = if explained.hinted { ", as hinted" } else { "" };
    let _ = writeln!(text, "plan: {plan}{hinted}");
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
    // The plans passed over (SPEC §133.5), after what the run did.
    for other in &explained.alternatives {
        let entries = |n: u64| match n {
            1 => "1 entry".to_string(),
            n => format!("{n} entries"),
        };
        let entries = match other.entries {
            EntryCount::Exactly(n) => entries(n),
            EntryCount::MoreThan(n) => format!("more than {}", entries(n)),
            _ => format!("{:?}", other.entries),
        };
        let _ = write!(text, "instead: {}, {entries}", plan_words(&other.plan));
        if !other.reads_documents {
            text.push_str(", its entries decide the filter");
        }
        if other.fewer {
            text.push_str(", fewer");
        }
        text.push('\n');
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

/// A plan in words, as `--explain` names it (SPEC §113).
fn plan_words(plan: &trunkdb::query::QueryPlan) -> String {
    use trunkdb::query::QueryPlan;
    match plan {
        QueryPlan::Scan => "scan of every document".to_string(),
        QueryPlan::Index { field, .. } => format!("index on {field}"),
        QueryPlan::IndexOrder { field, .. } => format!("index on {field}, in its order"),
        // Ids looked up beside index ranges, `_id` first (SPEC §164.4).
        QueryPlan::IndexUnion { fields, .. } => match fields.split_first() {
            Some((first, [only])) if first == "_id" => {
                format!("ids looked up, and the index on {only}, together")
            }
            Some((first, rest)) if first == "_id" => {
                format!(
                    "ids looked up, and the indexes on {}, together",
                    rest.join(", ")
                )
            }
            _ => format!("indexes on {}, together", fields.join(", ")),
        },
        QueryPlan::ById => "by id".to_string(),
        other => format!("{other:?}"),
    }
}

/// At most this many paths are kept (SPEC §121.4): documents whose keys
/// are data, ids or dates as names, would otherwise grow the summary with
/// every document.
const MAX_PATHS: usize = 10_000;

/// The kinds of value a path can hold, in the order a tie between two
/// counts is printed; the names are the ones the filter language and
/// `Display` use for them.
const KINDS: [&str; 10] = [
    "null", "bool", "int", "float", "string", "binary", "array", "object", "id", "date",
];

fn kind(value: &Document) -> usize {
    match value {
        Document::Null => 0,
        Document::Bool(_) => 1,
        Document::Int(_) => 2,
        Document::Float(_) => 3,
        Document::String(_) => 4,
        Document::Binary(_) => 5,
        Document::Array(_) => 6,
        Document::Object(_) => 7,
        Document::Id(_) => 8,
        Document::DateTime(_) => 9,
        _ => 0,
    }
}

/// One path of the summary (SPEC §121.2), and the paths below it: the
/// fields of the objects found there, and the elements of its arrays.
/// Every count is of documents, so a document counts once per path and
/// once per kind however many values it has there.
#[derive(Default)]
struct Shape {
    /// Documents with a value here.
    documents: u64,
    /// Documents with a value of each kind here.
    kinds: [u64; KINDS.len()],
    /// The number, from 1, of the last document counted, for `documents`
    /// and for each kind: a document reaches a path under an array once
    /// per element.
    last: u64,
    last_kind: [u64; KINDS.len()],
    fields: std::collections::BTreeMap<String, Shape>,
    elements: Option<Box<Shape>>,
}

/// The whole summary: the documents' fields, and how many paths it keeps.
struct Fields {
    root: Shape,
    paths: usize,
    /// Whether a path was left out at `MAX_PATHS`.
    full: bool,
}

impl Fields {
    fn add(&mut self, doc: &Document, number: u64) {
        if let Document::Object(map) = doc {
            for (name, value) in map.iter() {
                Self::field(
                    &mut self.root,
                    name,
                    value,
                    number,
                    &mut self.paths,
                    &mut self.full,
                );
            }
        }
    }

    /// `value`, the field `name` of an object at `parent`.
    fn field(
        parent: &mut Shape,
        name: &str,
        value: &Document,
        number: u64,
        paths: &mut usize,
        full: &mut bool,
    ) {
        if !parent.fields.contains_key(name) {
            if *paths == MAX_PATHS {
                *full = true;
                return;
            }
            *paths += 1;
            parent.fields.insert(name.to_string(), Shape::default());
        }
        let shape = parent.fields.get_mut(name).expect("just found or made");
        Self::value(shape, value, number, paths, full);
    }

    /// `value`, counted at `shape`, and what's inside it below.
    fn value(shape: &mut Shape, value: &Document, number: u64, paths: &mut usize, full: &mut bool) {
        if shape.last != number {
            shape.last = number;
            shape.documents += 1;
        }
        let kind = kind(value);
        if shape.last_kind[kind] != number {
            shape.last_kind[kind] = number;
            shape.kinds[kind] += 1;
        }
        match value {
            Document::Object(map) => {
                for (name, value) in map.iter() {
                    Self::field(shape, name, value, number, paths, full);
                }
            }
            Document::Array(items) => {
                for item in items {
                    if shape.elements.is_none() {
                        if *paths == MAX_PATHS {
                            *full = true;
                            return;
                        }
                        *paths += 1;
                        shape.elements = Some(Box::default());
                    }
                    let elements = shape.elements.as_mut().expect("just made");
                    Self::value(elements, item, number, paths, full);
                }
            }
            _ => {}
        }
    }

    /// Each path with its counts, `_id` first, then a field before what's
    /// below it, names in order, an object's fields before its elements.
    fn lines(&self) -> Vec<Line<'_>> {
        let mut lines = Vec::new();
        let mut names: Vec<&String> = self.root.fields.keys().collect();
        names.sort_by_key(|name| name.as_str() != "_id");
        for name in names {
            walk(
                name_text(name),
                named(name),
                &self.root.fields[name],
                &mut lines,
            );
        }
        lines
    }
}

/// A path of the summary, as a filter writes it.
struct Line<'s> {
    text: String,
    /// Whether a filter can name it: no name on the way has a `.` or a
    /// `[*]` in it (SPEC §31.2).
    nameable: bool,
    shape: &'s Shape,
}

fn named(name: &str) -> bool {
    !name.contains('.') && !name.contains("[*]")
}

/// `name` as a filter writes it (SPEC §111.3), in backticks unless it's a
/// bare word: written by the filter language itself, so the two can't
/// disagree. One no path can name in backticks too, which the language
/// would take apart at its `.`.
fn name_text(name: &str) -> String {
    match named(name) {
        true => {
            let text = trunkdb::query::Condition::exists(name).to_string();
            text["exists(".len()..text.len() - 1].to_string()
        }
        false => format!("`{}`", name.replace('`', "``")),
    }
}

fn walk<'s>(text: String, nameable: bool, shape: &'s Shape, lines: &mut Vec<Line<'s>>) {
    lines.push(Line {
        text: text.clone(),
        nameable,
        shape,
    });
    for (name, field) in &shape.fields {
        let below = format!("{text}.{}", name_text(name));
        walk(below, nameable && named(name), field, lines);
    }
    if let Some(ref elements) = shape.elements {
        walk(format!("{text}[*]"), nameable, elements, lines);
    }
}

/// Every path the documents `filter` finds have (SPEC §121): in how many
/// documents, and as which kinds of value. How many documents, to stderr.
fn fields(db: &Database, collection: &str, filter: &str, given: &Given, format: Format) -> Outcome {
    let failed = |e: trunkdb::Error| e.to_string();
    let (docs, filter) = match query(db, collection, filter, given)? {
        Ok(query) => query,
        Err(usage) => return Ok(usage),
    };
    let mut summary = Fields {
        root: Shape::default(),
        paths: 0,
        full: false,
    };
    let mut read = 0;
    for doc in docs.cursor(filter).map_err(failed)? {
        read += 1;
        summary.add(&doc.map_err(failed)?, read);
    }
    if format == Format::Json {
        let mut out = io::BufWriter::new(io::stdout().lock());
        for line in summary.lines() {
            let mut kinds: Vec<usize> = (0..KINDS.len())
                .filter(|&k| line.shape.kinds[k] > 0)
                .collect();
            kinds.sort_by_key(|&k| std::cmp::Reverse(line.shape.kinds[k]));
            let kinds: serde_json::Map<String, serde_json::Value> = (kinds.iter())
                .map(|&k| (KINDS[k].to_string(), line.shape.kinds[k].into()))
                .collect();
            let value = serde_json::json!({
                "path": line.text,
                "nameable": line.nameable,
                "documents": line.shape.documents,
                "kinds": kinds,
            });
            if let Err(e) = writeln!(out, "{value}") {
                return stopped_writing(e);
            }
        }
        if let Err(e) = out.flush() {
            return stopped_writing(e);
        }
        eprintln!(
            "{} in {}",
            count(summary.lines().len(), "path"),
            count(read as usize, "document")
        );
        if summary.full {
            eprintln!("only the first {MAX_PATHS} paths found are counted; a filter can narrow it");
        }
        return Ok(ExitCode::SUCCESS);
    }
    let lines: Vec<(String, &Shape)> = summary
        .lines()
        .iter()
        .map(|line| match line.nameable {
            true => (line.text.clone(), line.shape),
            false => (format!("{} (no path can name it)", line.text), line.shape),
        })
        .collect();
    let width = lines
        .iter()
        .map(|(path, _)| path.chars().count())
        .max()
        .unwrap_or(0);
    let digits = read.to_string().len();
    let mut out = io::BufWriter::new(io::stdout().lock());
    for (path, shape) in &lines {
        let mut kinds: Vec<usize> = (0..KINDS.len()).filter(|&k| shape.kinds[k] > 0).collect();
        // Most first; a tie in `KINDS`' order.
        kinds.sort_by_key(|&k| std::cmp::Reverse(shape.kinds[k]));
        let kinds: Vec<String> = kinds
            .iter()
            .map(|&k| format!("{} {}", KINDS[k], shape.kinds[k]))
            .collect();
        let padding = width - path.chars().count();
        if let Err(e) = writeln!(
            out,
            "{path}{:padding$}  {:>digits$}  {}",
            "",
            shape.documents,
            kinds.join(", ")
        ) {
            return stopped_writing(e);
        }
    }
    if let Err(e) = out.flush() {
        return stopped_writing(e);
    }
    eprintln!(
        "{} in {}",
        count(lines.len(), "path"),
        count(read as usize, "document")
    );
    if summary.full {
        eprintln!("only the first {MAX_PATHS} paths found are counted; a filter can narrow it");
    }
    Ok(ExitCode::SUCCESS)
}

// Writing (SPEC §156).

/// A command line that doesn't say what to do: why, and exit code 2, as
/// for the usage.
fn misused(why: &str) -> Outcome {
    eprintln!("trunkdb: {why} (`trunkdb help` shows how)");
    Ok(ExitCode::from(2))
}

/// `args` split into the options among them, each one of `allowed`, and
/// the rest, in order. `Err` names one that isn't allowed.
fn options<'a>(args: &[&'a str], allowed: &[&str]) -> Result<(Vec<&'a str>, Vec<&'a str>), String> {
    let mut rest = Vec::new();
    let mut given = Vec::new();
    for &arg in args {
        match arg.strip_prefix("--") {
            Some(_) if allowed.contains(&arg) => given.push(arg),
            Some(_) => return Err(format!("{arg}: not an option of this command")),
            None => rest.push(arg),
        }
    }
    Ok((rest, given))
}

/// Whether `name` is a collection of `db`.
fn has_collection(db: &Database, name: &str) -> Result<bool, String> {
    Ok(db
        .collections()
        .map_err(|e| e.to_string())?
        .iter()
        .any(|c| c == name))
}

/// `trunkdb insert` (SPEC §156): one document given as JSON, or one per
/// line on standard input, all in one batch; each new id printed.
fn insert(file: &str, collection: &str, args: &[&str]) -> Outcome {
    let (rest, given) = match options(args, &["--create"]) {
        Ok(split) => split,
        Err(why) => return misused(&why),
    };
    let [source] = rest[..] else {
        return misused(
            "insert takes one document as JSON, or `-` to read them from standard input",
        );
    };
    let create = given.contains(&"--create");
    let mut docs = Vec::new();
    let mut add = |text: &str, line: Option<usize>| -> Result<(), String> {
        let at = line.map_or(String::new(), |line| format!("line {line}: "));
        let doc: Document = text
            .parse()
            .map_err(|e| format!("{at}not a JSON document: {e}"))?;
        if !matches!(doc, Document::Object(_)) {
            return Err(format!("{at}a document is a JSON object, `{{…}}`"));
        }
        docs.push(doc);
        Ok(())
    };
    let read = match source {
        "-" => {
            if io::stdin().is_terminal() {
                return misused(
                    "`-` reads documents from standard input, one per line; pipe them in",
                );
            }
            let mut read = Ok(());
            for (i, line) in io::stdin().lines().enumerate() {
                let line = line.map_err(|e| format!("standard input: {e}"))?;
                if line.trim().is_empty() {
                    continue;
                }
                read = add(&line, Some(i + 1));
                if read.is_err() {
                    break;
                }
            }
            read
        }
        text => add(text, None),
    };
    if let Err(why) = read {
        eprintln!("trunkdb: {why}; nothing inserted");
        return Ok(ExitCode::from(2));
    }
    let db = match create {
        true => open(file)?,
        false => existing(file)?,
    };
    if !create && !has_collection(&db, collection)? {
        return Err(format!(
            "{collection}: no such collection; --create makes it"
        ));
    }
    let failed = |e: trunkdb::Error| e.to_string();
    let target = db.collection::<Document>(collection);
    let mut batch = db.batch();
    let mut ids = Vec::with_capacity(docs.len());
    for doc in docs {
        ids.push(batch.insert(&target, doc).map_err(failed)?);
    }
    batch.commit().map_err(failed)?;
    let mut out = io::BufWriter::new(io::stdout().lock());
    for id in &ids {
        if let Err(e) = writeln!(out, "{id}") {
            return stopped_writing(e);
        }
    }
    if let Err(e) = out.flush() {
        return stopped_writing(e);
    }
    eprintln!("inserted {}", count(ids.len(), "document"));
    Ok(ExitCode::SUCCESS)
}

/// An update written as MongoDB writes one (SPEC §156.2): an object of
/// operators, each an object of paths and what to do there.
fn parse_update(text: &str) -> Result<trunkdb::query::Update, String> {
    let doc: Document = text
        .parse()
        .map_err(|e| format!("the update isn't JSON: {e}"))?;
    update_from(doc)
}

/// An update given as a document of operators, as `parse_update` reads
/// one: for `trunkdb batch`, where it is a value in a line (SPEC §168).
fn update_from(doc: Document) -> Result<trunkdb::query::Update, String> {
    use trunkdb::query::Update;
    const OPERATORS: &str = "$set $setOnInsert $unset $inc $mul $min $max $rename $push \
                             $addToSet $pull $pop";
    let Document::Object(ops) = doc else {
        return Err(
            r#"the update is an object of operators, like {"$set": {"status": "done"}}"#
                .to_string(),
        );
    };
    if ops.is_empty() {
        return Err(format!("the update has no operators; one of {OPERATORS}"));
    }
    let paths = |op: &str, args: Document| -> Result<Vec<(String, Document)>, String> {
        match args {
            Document::Object(fields) if !fields.is_empty() => Ok(fields.into_iter().collect()),
            _ => Err(format!(
                r#"{op} takes an object of paths, like {{"{op}": {{"status": …}}}}"#
            )),
        }
    };
    let mut update = Update::new();
    for (op, args) in ops {
        match op.as_str() {
            "$set" | "$setOnInsert" | "$inc" | "$mul" | "$min" | "$max" | "$pull" => {
                for (path, value) in paths(&op, args)? {
                    update = match op.as_str() {
                        "$set" => update.set(path, value),
                        "$setOnInsert" => update.set_on_insert(path, value),
                        "$inc" => update.inc(path, value),
                        "$mul" => update.mul(path, value),
                        "$min" => update.min(path, value),
                        "$max" => update.max(path, value),
                        _ => update.pull(path, value),
                    };
                }
            }
            "$unset" => {
                let names: Vec<String> = match args {
                    Document::Array(names) => names
                        .into_iter()
                        .map(|name| match name {
                            Document::String(name) => Ok(name),
                            _ => Err("$unset takes paths as strings".to_string()),
                        })
                        .collect::<Result<_, _>>()?,
                    args => paths(&op, args)?
                        .into_iter()
                        .map(|(path, _)| path)
                        .collect(),
                };
                for path in names {
                    update = update.unset(path);
                }
            }
            "$rename" => {
                for (from, to) in paths(&op, args)? {
                    let Document::String(to) = to else {
                        return Err(format!(
                            "$rename takes the new path as a string, for {from}"
                        ));
                    };
                    update = update.rename(from, to);
                }
            }
            "$push" | "$addToSet" => {
                for (path, value) in paths(&op, args)? {
                    let each = match &value {
                        Document::Object(fields) if fields.len() == 1 => {
                            match fields.get("$each") {
                                Some(Document::Array(values)) => Some(values.clone()),
                                Some(_) => {
                                    return Err(format!("{op}: $each takes an array, for {path}"));
                                }
                                None => None,
                            }
                        }
                        _ => None,
                    };
                    update = match (op.as_str(), each) {
                        ("$push", Some(values)) => update.push_all(path, values),
                        ("$push", None) => update.push(path, value),
                        (_, Some(values)) => values.into_iter().fold(update, |update, value| {
                            update.add_to_set(path.clone(), value)
                        }),
                        (_, None) => update.add_to_set(path, value),
                    };
                }
            }
            "$pop" => {
                for (path, end) in paths(&op, args)? {
                    update = match end {
                        Document::Int(1) => update.pop_last(path),
                        Document::Int(-1) => update.pop_first(path),
                        _ => {
                            return Err(format!(
                                "$pop takes 1 for the last element or -1 for the first, for {path}"
                            ));
                        }
                    };
                }
            }
            other if other.starts_with('$') => {
                return Err(format!(
                    "{other}: not an update operator; one of {OPERATORS}"
                ));
            }
            field => {
                return Err(format!(
                    r#"`{field}` isn't an operator: say how to change it, {{"$set": {{"{field}": …}}}}; a whole document isn't replaced"#
                ));
            }
        }
    }
    Ok(update)
}

/// A write whose filter takes every document of the collection, more than
/// one, without `--all` (SPEC §156.3): refused, saying so.
fn touches_all(
    docs: &trunkdb::Collection<Document>,
    filter: &Filter,
    all: bool,
    verb: &str,
) -> Result<Option<ExitCode>, String> {
    if all {
        return Ok(None);
    }
    let failed = |e: trunkdb::Error| e.to_string();
    let total = docs.count(Filter::new()).map_err(failed)?;
    let taken = docs.count(filter.clone()).map_err(failed)?;
    if total > 1 && taken == total {
        eprintln!(
            "trunkdb: this would {verb} all {total} documents of {}; add --all if that's meant",
            docs.name()
        );
        return Ok(Some(ExitCode::from(2)));
    }
    Ok(None)
}

/// `trunkdb update` (SPEC §156): the documents the filter finds changed by
/// the update's operators; with `--upsert`, one inserted where none is.
fn update(file: &str, collection: &str, args: &[&str], values: &Given) -> Outcome {
    let (rest, given) = match options(args, &["--all", "--upsert", "--dry-run"]) {
        Ok(split) => split,
        Err(why) => return misused(&why),
    };
    let [filter, update] = rest[..] else {
        return misused("update takes a filter and an update, each one argument in quotes");
    };
    let update = match parse_update(update) {
        Ok(update) => update,
        Err(why) => return misused(&why),
    };
    let db = existing(file)?;
    let (docs, filter) = match query(&db, collection, filter, values)? {
        Ok(query) => query,
        Err(usage) => return Ok(usage),
    };
    let failed = |e: trunkdb::Error| e.to_string();
    let dry_run = given.contains(&"--dry-run");
    if given.contains(&"--upsert") {
        let found = docs.count(filter.clone()).map_err(failed)?;
        if dry_run {
            eprintln!(
                "would {}",
                match found {
                    0 => "insert 1 document".to_string(),
                    n => format!("update {}", count(n, "document")),
                }
            );
            return Ok(ExitCode::SUCCESS);
        }
        let upserted = docs.upsert_fields(filter, &update).map_err(failed)?;
        println!("{}", upserted.id());
        eprintln!(
            "{} 1 document",
            match upserted {
                trunkdb::Upserted::Inserted(_) => "inserted",
                _ => "updated",
            }
        );
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(refused) = touches_all(&docs, &filter, given.contains(&"--all"), "update")? {
        return Ok(refused);
    }
    if dry_run {
        let n = docs.count(filter).map_err(failed)?;
        eprintln!("would update {}", count(n, "document"));
        return Ok(ExitCode::SUCCESS);
    }
    let n = docs.update_fields(filter, &update).map_err(failed)?;
    eprintln!("updated {}", count(n, "document"));
    Ok(ExitCode::SUCCESS)
}

/// `trunkdb delete` (SPEC §156): the documents the filter finds.
fn delete(file: &str, collection: &str, args: &[&str], values: &Given) -> Outcome {
    let (rest, given) = match options(args, &["--all", "--dry-run"]) {
        Ok(split) => split,
        Err(why) => return misused(&why),
    };
    let [filter] = rest[..] else {
        return misused("delete takes a filter, one argument in quotes");
    };
    let db = existing(file)?;
    let (docs, filter) = match query(&db, collection, filter, values)? {
        Ok(query) => query,
        Err(usage) => return Ok(usage),
    };
    let failed = |e: trunkdb::Error| e.to_string();
    if let Some(refused) = touches_all(&docs, &filter, given.contains(&"--all"), "delete")? {
        return Ok(refused);
    }
    if given.contains(&"--dry-run") {
        let n = docs.count(filter).map_err(failed)?;
        eprintln!("would delete {}", count(n, "document"));
        return Ok(ExitCode::SUCCESS);
    }
    let n = docs.delete_many(filter).map_err(failed)?;
    eprintln!("deleted {}", count(n, "document"));
    Ok(ExitCode::SUCCESS)
}

/// One line of `trunkdb batch` (SPEC §168), read and checked.
enum Step {
    Insert(Document),
    Update(Filter, trunkdb::query::Update),
    Delete(Filter),
    Require(Filter, bool),
}

/// A line's op, with the collection it names and its line number.
struct BatchOp {
    line: usize,
    collection: String,
    op: Step,
}

/// `text`, a line of a batch, as an op: a JSON object with `op` and
/// `collection`, and what that op takes. `Err(Some(why))` is a mistake
/// to print; `Err(None)` one already printed, as `bound` prints a
/// filter's.
fn batch_line(text: &str, given: &Given, line: usize) -> Result<BatchOp, Option<String>> {
    let doc: Document = text
        .parse()
        .map_err(|e| Some(format!("not a JSON object: {e}")))?;
    let Document::Object(mut fields) = doc else {
        return Err(Some(
            r#"an op is a JSON object, like {"op": "delete", "collection": "tasks", "filter": "done = true"}"#
                .to_string(),
        ));
    };
    let mut take = |name: &str| fields.remove(name);
    let op = match take("op") {
        Some(Document::String(op)) => op,
        Some(_) => return Err(Some("`op` is a string".to_string())),
        None => {
            return Err(Some(
                "no `op`: one of insert, update, delete, require, require_none".to_string(),
            ));
        }
    };
    let collection = match take("collection") {
        Some(Document::String(name)) if !name.is_empty() => name,
        Some(_) => {
            return Err(Some(
                "`collection` is a collection's name, a string".to_string(),
            ));
        }
        None => return Err(Some(format!("{op}: no `collection`"))),
    };
    let filter_text = take("filter");
    let filter = |what: &str| -> Result<(Filter, bool), Option<String>> {
        let text = match filter_text.clone() {
            Some(Document::String(text)) => text,
            Some(_) => {
                return Err(Some(format!(
                    "{what}: `filter` is a filter's text, a string, \"\" for none"
                )));
            }
            None => {
                return Err(Some(format!(
                    "{what}: no `filter`; \"\" takes every document"
                )));
            }
        };
        let parsed = bound(
            &format!("line {line}: filter"),
            &text,
            given,
            Template::<Filter>::bind,
        )
        .map_err(|_| None)?;
        Ok((parsed, text.trim().is_empty()))
    };
    let op = match op.as_str() {
        "insert" => match take("document") {
            Some(doc @ Document::Object(_)) => Step::Insert(doc),
            Some(_) => return Err(Some("insert: `document` is a JSON object".to_string())),
            None => return Err(Some("insert: no `document`".to_string())),
        },
        "update" | "delete" => {
            let (parsed, empty) = filter(&op)?;
            let all = match take("all") {
                Some(Document::Bool(all)) => all,
                None => false,
                Some(_) => return Err(Some(format!("{op}: `all` is true or false"))),
            };
            if empty && !all {
                return Err(Some(format!(
                    "{op}: the filter is empty, which takes every document of {collection}; add \"all\": true if that's meant"
                )));
            }
            match op.as_str() {
                "update" => {
                    let update = match take("update") {
                        Some(update) => update_from(update).map_err(Some)?,
                        None => return Err(Some(
                            r#"update: no `update`, the operators, like {"$set": {"status": "done"}}"#
                                .to_string(),
                        )),
                    };
                    Step::Update(parsed, update)
                }
                _ => Step::Delete(parsed),
            }
        }
        "require" | "require_none" => Step::Require(filter(&op)?.0, op == "require_none"),
        other => {
            return Err(Some(format!(
                "{other}: not an op; one of insert, update, delete, require, require_none"
            )));
        }
    };
    if filter_text.is_some() && matches!(op, Step::Insert(_)) {
        return Err(Some("insert: takes no `filter`".to_string()));
    }
    if let Some(name) = fields.keys().next() {
        return Err(Some(format!("`{name}`: not a field of an op line")));
    }
    Ok(BatchOp {
        line,
        collection,
        op,
    })
}

/// `ops` as a batch of `db`; `Err` names the line whose op is refused as
/// written, as a precondition with a limit is.
fn batch_of(
    db: &Database,
    ops: &[BatchOp],
) -> Result<(trunkdb::Batch, Vec<trunkdb::DocId>), String> {
    let mut batch = db.batch();
    let mut ids = Vec::new();
    for op in ops {
        let target = db.collection::<Document>(&op.collection);
        let added = match &op.op {
            Step::Insert(doc) => batch.insert(&target, doc.clone()).map(|id| ids.push(id)),
            Step::Update(filter, update) => batch.update_many(&target, filter.clone(), update),
            Step::Delete(filter) => {
                batch.delete_many(&target, filter.clone());
                Ok(())
            }
            Step::Require(filter, false) => batch.require(&target, filter.clone()),
            Step::Require(filter, true) => batch.require_none(&target, filter.clone()),
        };
        added.map_err(|e| format!("line {}: {e}", op.line))?;
    }
    Ok((batch, ids))
}

/// The line of the op a batch failed at: a precondition's own, any other
/// error's by dry runs of ever shorter beginnings of the batch, the
/// first that fails ending at it. The file is locked while open (SPEC
/// §27), so nothing else writes between them.
fn failed_line(db: &Database, ops: &[BatchOp], error: &trunkdb::Error) -> Option<usize> {
    if let trunkdb::Error::PreconditionFailed { op, .. } = error {
        return ops.get(*op).map(|op| op.line);
    }
    // A beginning of `ok` ops runs; one of `fails` doesn't.
    let (mut ok, mut fails) = (0, ops.len());
    while fails - ok > 1 {
        let mid = (ok + fails) / 2;
        match batch_of(db, &ops[..mid]).ok()?.0.dry_run() {
            Ok(_) => ok = mid,
            Err(_) => fails = mid,
        }
    }
    Some(ops[fails - 1].line)
}

/// `trunkdb batch` (SPEC §168): one op a line, as JSON, all committed in
/// one batch, or none of them.
fn batch(file: &str, args: &[&str], values: &Given) -> Outcome {
    use std::io::BufRead;
    let (rest, given) = match options(args, &["--create", "--dry-run"]) {
        Ok(split) => split,
        Err(why) => return misused(&why),
    };
    let [source] = rest[..] else {
        return misused(
            "batch takes a file of ops, one JSON object a line, or `-` for standard input",
        );
    };
    let (create, dry_run) = (given.contains(&"--create"), given.contains(&"--dry-run"));
    let lines: Box<dyn BufRead> = match source {
        "-" if io::stdin().is_terminal() => {
            return misused("`-` reads the ops from standard input, one per line; pipe them in");
        }
        "-" => Box::new(io::stdin().lock()),
        path => match std::fs::File::open(path) {
            Ok(opened) => Box::new(io::BufReader::new(opened)),
            Err(e) => return Err(format!("{path}: {e}")),
        },
    };
    let mut ops = Vec::new();
    for (i, text) in lines.lines().enumerate() {
        let text = text.map_err(|e| format!("{source}: {e}"))?;
        if text.trim().is_empty() {
            continue;
        }
        match batch_line(&text, values, i + 1) {
            Ok(op) => ops.push(op),
            Err(why) => {
                if let Some(why) = why {
                    eprintln!("trunkdb: line {}: {why}; nothing written", i + 1);
                }
                return Ok(ExitCode::from(2));
            }
        }
    }
    if let Some(name) = values.unused() {
        eprintln!("trunkdb: --param {name}: no filter of the batch has the parameter :{name}");
        return Ok(ExitCode::from(2));
    }
    let db = match create {
        true => open(file)?,
        false => existing(file)?,
    };
    if !create {
        let names = db.collections().map_err(|e| e.to_string())?;
        if let Some(op) = ops.iter().find(|op| !names.contains(&op.collection)) {
            return Err(format!(
                "line {}: {}: no such collection; --create makes it; nothing written",
                op.line, op.collection
            ));
        }
    }
    if ops.is_empty() {
        eprintln!("no ops; nothing written");
        return Ok(ExitCode::SUCCESS);
    }
    let (batch, ids) = match batch_of(&db, &ops) {
        Ok(built) => built,
        Err(why) => {
            eprintln!("trunkdb: {why}; nothing written");
            return Ok(ExitCode::from(2));
        }
    };
    let outcome = match dry_run {
        true => batch.dry_run(),
        false => batch.commit(),
    };
    let written = match outcome {
        Ok(written) => written,
        Err(e) => {
            let at =
                failed_line(&db, &ops, &e).map_or(String::new(), |line| format!("line {line}: "));
            return Err(format!("{at}{e}; nothing written"));
        }
    };
    if !dry_run {
        // The new ids, as `trunkdb insert` prints them: a script reads them.
        let mut out = io::BufWriter::new(io::stdout().lock());
        for id in &ids {
            if let Err(e) = writeln!(out, "{id}") {
                return stopped_writing(e);
            }
        }
        if let Err(e) = out.flush() {
            return stopped_writing(e);
        }
    }
    let would = if dry_run { "would " } else { "" };
    let (mut inserted, mut updated, mut deleted) = (0, 0, 0);
    for (op, &n) in ops.iter().zip(written.counts()) {
        let verb = match op.op {
            Step::Insert(_) => {
                inserted += n;
                continue;
            }
            Step::Update(..) => {
                updated += n;
                if dry_run { "update" } else { "updated" }
            }
            Step::Delete(_) => {
                deleted += n;
                if dry_run { "delete" } else { "deleted" }
            }
            Step::Require(..) => continue,
        };
        eprintln!("line {}: {would}{verb} {}", op.line, count(n, "document"));
    }
    let done: Vec<String> = [
        ("insert", "inserted", inserted),
        ("update", "updated", updated),
        ("delete", "deleted", deleted),
    ]
    .iter()
    .filter(|(.., n)| *n > 0)
    .map(|&(verb, past, n)| {
        let verb = if dry_run { verb } else { past };
        format!("{verb} {}", count(n, "document"))
    })
    .collect();
    let done = match done.as_slice() {
        [] if dry_run => "write nothing".to_string(),
        [] => "wrote nothing".to_string(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    };
    match dry_run {
        true => eprintln!("{} would {done}; nothing written", count(ops.len(), "op")),
        false => eprintln!("committed {}: {done}", count(ops.len(), "op")),
    }
    Ok(ExitCode::SUCCESS)
}

/// An index's fields as `info` names them: `status`, `(team, email)`.
fn index_name(fields: &[&str]) -> String {
    match fields {
        [one] => one.to_string(),
        many => format!("({})", many.join(", ")),
    }
}

/// `trunkdb index … add` (SPEC §156): an index made, over the documents
/// already there, or found there already.
fn index_add(file: &str, collection: &str, args: &[&str]) -> Outcome {
    let (fields, given) = match options(args, &["--unique", "--sparse"]) {
        Ok(split) => split,
        Err(why) => return misused(&why),
    };
    if fields.is_empty() {
        return misused("index add takes the fields to index, one or more");
    }
    let db = existing(file)?;
    if !has_collection(&db, collection)? {
        return Err(format!("{collection}: no such collection"));
    }
    let mut options = trunkdb::IndexOptions::new();
    if given.contains(&"--unique") {
        options = options.unique();
    }
    if given.contains(&"--sparse") {
        options = options.sparse();
    }
    let made = db
        .collection::<Document>(collection)
        .ensure_index_with(&fields[..], options)
        .map_err(|e| e.to_string())?;
    let name = index_name(&fields);
    match made {
        true => eprintln!("added index {name} to {collection}"),
        false => eprintln!("{collection} has index {name} already"),
    }
    Ok(ExitCode::SUCCESS)
}

/// `trunkdb index … drop` (SPEC §156); none on those fields is exit code 1.
fn index_drop(file: &str, collection: &str, args: &[&str]) -> Outcome {
    let (fields, _) = match options(args, &[]) {
        Ok(split) => split,
        Err(why) => return misused(&why),
    };
    if fields.is_empty() {
        return misused("index drop takes the fields of the index, one or more");
    }
    let db = existing(file)?;
    if !has_collection(&db, collection)? {
        return Err(format!("{collection}: no such collection"));
    }
    let dropped = db
        .collection::<Document>(collection)
        .drop_index(&fields[..])
        .map_err(|e| e.to_string())?;
    let name = index_name(&fields);
    match dropped {
        true => {
            eprintln!("dropped index {name} of {collection}");
            Ok(ExitCode::SUCCESS)
        }
        false => Err(format!("{collection} has no index {name}")),
    }
}

/// `trunkdb drop` (SPEC §156): a collection, its documents and indexes.
fn drop_collection(file: &str, collection: &str) -> Outcome {
    let db = existing(file)?;
    if !has_collection(&db, collection)? {
        return Err(format!("{collection}: no such collection"));
    }
    let failed = |e: trunkdb::Error| e.to_string();
    let n = db
        .collection::<Document>(collection)
        .count(Filter::new())
        .map_err(failed)?;
    db.drop_collection(collection).map_err(failed)?;
    eprintln!("dropped {collection} and its {}", count(n, "document"));
    Ok(ExitCode::SUCCESS)
}
