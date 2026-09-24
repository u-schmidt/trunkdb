//! Export and import: the whole database as JSON Lines (SPEC §30).
//!
//! ```text
//! {"$trunkdb_export":1}
//! {"$collection":"users","$indexes":["age"]}
//! {"name":"Ada","age":36,"_id":{"$id":"0199…"}}
//! {"name":"Bob","age":41,"_id":{"$id":"0199…"}}
//! {"$collection":"pings","$indexes":[]}
//! …
//! ```
//!
//! One header line, then for each collection a line naming it and its
//! indexed fields, followed by its documents, one per line, in tagged
//! JSON (`json.rs`). The file is independent of the page layout, so it's
//! also how data moves from one file format version to the next.

use crate::collection::get_or_create_meta;
use crate::database::Database;
use crate::id::IdGenerator;
use crate::index::{BTreeIndex, Index};
use crate::json::{document_line, parse_document_line};
use crate::txn::WriteOp;
use crate::{Error, data};
use serde_json::{Map, Value};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};

/// The version of the export format itself — not the file format
/// (SPEC §21.2), which an export deliberately doesn't depend on.
const EXPORT_VERSION: u64 = 1;
const HEADER_KEY: &str = "$trunkdb_export";
const COLLECTION_KEY: &str = "$collection";
const INDEXES_KEY: &str = "$indexes";
const FIELD_KEY: &str = "field";
const UNIQUE_KEY: &str = "unique";

/// An import writes a batch at most this many documents…
const CHUNK_DOCUMENTS: usize = 1000;
/// …or about this many bytes of JSON, whichever comes first: a batch
/// holds every page it changes in memory until it commits (SPEC §19.9).
const CHUNK_BYTES: usize = 8 << 20;

/// What an export wrote or an import read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Summary {
    pub collections: usize,
    pub documents: usize,
}

impl Database {
    /// The names of all collections, sorted.
    pub fn collections(&self) -> crate::Result<Vec<String>> {
        let state = self.read()?;
        let mut names: Vec<String> = state.catalog.names().map(str::to_string).collect();
        names.sort();
        Ok(names)
    }

    /// Writes every collection — its name, its indexed fields and all its
    /// documents with their ids — to `out` as JSON Lines, collections in
    /// name order, documents in id order.
    ///
    /// A consistent snapshot: the read lock is held for the whole export,
    /// so no write lands halfway through (writers wait until it's done;
    /// readers don't). `out` is buffered here; it must not write to this
    /// database itself, which would deadlock.
    pub fn export(&self, out: impl Write) -> crate::Result<Summary> {
        let mut out = BufWriter::new(out);
        let mut summary = Summary::default();
        let state = self.read()?;

        write_line(&mut out, &object([(HEADER_KEY, EXPORT_VERSION.into())]))?;
        let mut names: Vec<&str> = state.catalog.names().collect();
        names.sort();
        for name in names {
            let indexes = state
                .catalog
                .indexes(name)
                .iter()
                .map(|index| match index.unique {
                    false => Value::String(index.field.clone()),
                    true => object([
                        (FIELD_KEY, index.field.clone().into()),
                        (UNIQUE_KEY, true.into()),
                    ]),
                })
                .collect();
            let header = object([
                (COLLECTION_KEY, name.into()),
                (INDEXES_KEY, Value::Array(indexes)),
            ]);
            write_line(&mut out, &header)?;
            summary.collections += 1;

            let meta = state.catalog.get(name).expect("a listed collection");
            let mut records = data::Records::new(&state.store);
            for (_key, loc) in BTreeIndex::new(meta.index_root).scan(&state.store)? {
                let (id, doc) = records.get(loc)?;
                write_line(&mut out, &document_line(id, &doc))?;
                summary.documents += 1;
            }
        }
        drop(state);
        out.flush()?;
        Ok(summary)
    }

    /// Reads an export (or a hand-written file in the same shape) from
    /// `input` into this database, keeping every document's id; a
    /// document line without an `_id` gets a new one.
    ///
    /// Not atomic: documents are written in batches of about 1000
    /// (SPEC §30.3). If a line is invalid, or an id already exists
    /// (`Error::DuplicateId`), the import stops, and everything written
    /// before that batch stays. Importing into a new, empty file and
    /// switching over only when it succeeds is the safe way to use it.
    /// Each collection's indexes are built once its documents are in.
    pub fn import(&self, input: impl Read) -> crate::Result<Summary> {
        let mut summary = Summary::default();
        let mut seen_header = false;
        let mut current: Option<CollectionHeader> = None;
        let mut chunk = Chunk::default();

        for (number, line) in BufReader::new(input).lines().enumerate() {
            let number = number + 1;
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let fail = |message: String| Error::Import {
                line: number,
                message,
            };
            let value: Value =
                serde_json::from_str(&line).map_err(|e| fail(format!("not JSON: {e}")))?;

            if !seen_header {
                check_header(&value).map_err(fail)?;
                seen_header = true;
                continue;
            }
            if let Some(header) = CollectionHeader::parse(&value).map_err(fail)? {
                chunk.write(self)?;
                if let Some(done) = current.take() {
                    done.build_indexes(self)?;
                }
                let name = header.name.clone();
                self.transact(|catalog, store| {
                    get_or_create_meta(catalog, store, &name).map(|_| ())
                })?;
                summary.collections += 1;
                current = Some(header);
                continue;
            }

            let Some(collection) = &current else {
                return Err(fail(format!(
                    "a document before the first {{\"{COLLECTION_KEY}\": ...}} line"
                )));
            };
            let (id, doc) = parse_document_line(value).map_err(|e| fail(e.to_string()))?;
            let id = id.unwrap_or_else(|| self.id_gen().generate());
            chunk.push(
                WriteOp::Insert(collection.name.clone(), id, doc),
                line.len(),
            );
            summary.documents += 1;
            if chunk.is_full() {
                chunk.write(self)?;
            }
        }

        if !seen_header {
            return Err(Error::Import {
                line: 1,
                message: "empty input: no header line".into(),
            });
        }
        chunk.write(self)?;
        if let Some(done) = current {
            done.build_indexes(self)?;
        }
        Ok(summary)
    }
}

/// The documents an import has read but not yet written.
#[derive(Default)]
struct Chunk {
    ops: Vec<WriteOp>,
    bytes: usize,
}

impl Chunk {
    fn push(&mut self, op: WriteOp, bytes: usize) {
        self.ops.push(op);
        self.bytes += bytes;
    }

    fn is_full(&self) -> bool {
        self.ops.len() >= CHUNK_DOCUMENTS || self.bytes >= CHUNK_BYTES
    }

    /// Writes the chunk as one batch and empties it.
    fn write(&mut self, db: &Database) -> crate::Result<()> {
        self.bytes = 0;
        db.write_batch(std::mem::take(&mut self.ops))
    }
}

/// A `{"$collection": ..., "$indexes": [...]}` line. An index is its
/// field as a string, or `{"field": ..., "unique": true}` (SPEC §33.5).
struct CollectionHeader {
    name: String,
    indexes: Vec<(String, bool)>,
}

impl CollectionHeader {
    /// `None` if `value` is a document line instead: one without
    /// `$collection`, or with an `_id` (an exported document always has
    /// one, so a document with a `$collection` field isn't misread).
    fn parse(value: &Value) -> Result<Option<Self>, String> {
        let Value::Object(object) = value else {
            return Ok(None);
        };
        if !object.contains_key(COLLECTION_KEY) || object.contains_key("_id") {
            return Ok(None);
        }
        let Some(Value::String(name)) = object.get(COLLECTION_KEY) else {
            return Err(format!("`{COLLECTION_KEY}` must be a string"));
        };
        let indexes = match object.get(INDEXES_KEY) {
            None => Vec::new(),
            Some(Value::Array(indexes)) => {
                indexes.iter().map(parse_index).collect::<Result<_, _>>()?
            }
            Some(other) => return Err(format!("`{INDEXES_KEY}` must be an array, got {other}")),
        };
        if let Some(key) = object
            .keys()
            .find(|k| *k != COLLECTION_KEY && *k != INDEXES_KEY)
        {
            return Err(format!("unknown key {key:?} in a collection line"));
        }
        Ok(Some(CollectionHeader {
            name: name.clone(),
            indexes,
        }))
    }

    fn build_indexes(self, db: &Database) -> crate::Result<()> {
        let collection = db.collection::<crate::Document>(&self.name);
        for (field, unique) in &self.indexes {
            match unique {
                false => collection.ensure_index(field)?,
                true => collection.ensure_unique_index(field)?,
            };
        }
        Ok(())
    }
}

/// One `$indexes` entry: `"field"`, or `{"field": "...", "unique":
/// bool}`.
fn parse_index(index: &Value) -> Result<(String, bool), String> {
    let wrong = || {
        format!(
            "an `{INDEXES_KEY}` entry must be a field name or \
             {{\"{FIELD_KEY}\": ..., \"{UNIQUE_KEY}\": true}}, got {index}"
        )
    };
    match index {
        Value::String(field) => Ok((field.clone(), false)),
        Value::Object(object) => {
            let Some(Value::String(field)) = object.get(FIELD_KEY) else {
                return Err(wrong());
            };
            let unique = match object.get(UNIQUE_KEY) {
                None => false,
                Some(Value::Bool(unique)) => *unique,
                Some(_) => return Err(wrong()),
            };
            if object.keys().any(|k| k != FIELD_KEY && k != UNIQUE_KEY) {
                return Err(wrong());
            }
            Ok((field.clone(), unique))
        }
        _ => Err(wrong()),
    }
}

fn check_header(value: &Value) -> Result<(), String> {
    let expected = format!("the first line must be {{\"{HEADER_KEY}\": {EXPORT_VERSION}}}");
    let Value::Object(object) = value else {
        return Err(expected);
    };
    match object.get(HEADER_KEY).and_then(Value::as_u64) {
        Some(EXPORT_VERSION) if object.len() == 1 => Ok(()),
        Some(version) if object.len() == 1 => Err(format!(
            "export format version {version} is not supported (this trunkdb reads {EXPORT_VERSION})"
        )),
        _ => Err(expected),
    }
}

fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect::<Map<_, _>>(),
    )
}

fn write_line(out: &mut impl Write, value: &Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *out, value)?;
    out.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{DocId, Document};
    use crate::query::{Condition, Filter, Op};
    use crate::testing::XorShift;

    fn open(dir: &tempfile::TempDir, name: &str) -> Database {
        Database::open(dir.path().join(name)).unwrap()
    }

    fn export_text(db: &Database) -> String {
        let mut out = Vec::new();
        db.export(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn import_text(db: &Database, text: &str) -> crate::Result<Summary> {
        db.import(text.as_bytes())
    }

    fn object(fields: &[(&str, Document)]) -> Document {
        Document::Object(
            fields
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    /// Every collection's documents with their ids, and its indexes —
    /// `NaN` compared by encoding, since `NaN != NaN`.
    /// A collection's name, indexed fields, and encoded documents by id.
    type Contents = (String, Vec<String>, Vec<(DocId, Vec<u8>)>);

    fn contents(db: &Database) -> Vec<Contents> {
        db.collections()
            .unwrap()
            .into_iter()
            .map(|name| {
                let collection = db.collection::<Document>(&name);
                let mut docs: Vec<_> = collection
                    .find_with_ids(Filter::default())
                    .unwrap()
                    .into_iter()
                    .map(|(id, doc)| (id, crate::document::encode_document(&doc)))
                    .collect();
                docs.sort();
                let mut indexes = collection.indexes().unwrap();
                let unique = collection.unique_indexes().unwrap();
                indexes.extend(unique.into_iter().map(|field| format!("unique: {field}")));
                (name, indexes, docs)
            })
            .collect()
    }

    /// Same collections, indexes, ids and documents — naming the first
    /// document that differs, decoded, instead of dumping two encodings.
    fn assert_same_contents(got: &Database, want: &Database) {
        for db in [got, want] {
            let report = db.check().unwrap();
            assert!(report.is_ok(), "{:#?}", report.problems);
        }
        let (got, want) = (contents(got), contents(want));
        let names = |c: &[Contents]| {
            c.iter()
                .map(|(n, i, d)| (n.clone(), i.clone(), d.len()))
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&got), names(&want));
        for ((name, _, got), (_, _, want)) in got.iter().zip(&want) {
            for ((id, a), (want_id, b)) in got.iter().zip(want) {
                if (id, a) != (want_id, b) {
                    let decode = |bytes| crate::document::decode_document(bytes).unwrap().0;
                    panic!(
                        "{name} {id} (want {want_id}):\n got  {:?}\n want {:?}",
                        decode(a),
                        decode(b)
                    );
                }
            }
        }
    }

    /// A random value of any kind, nested up to `depth`.
    fn random_value(rng: &mut XorShift, depth: usize) -> Document {
        let kinds = if depth == 0 { 8 } else { 10 };
        match rng.below(kinds) {
            0 => Document::Null,
            1 => Document::Bool(rng.below(2) == 1),
            2 => Document::Int(rng.next() as i64),
            3 => Document::Float([f64::NAN, f64::INFINITY, -0.0, 0.1, 1e300, -2.5][rng.below(6)]),
            4 => Document::String(["", "ß", "a\"b\\c\n", "\0x", "$id"][rng.below(5)].into()),
            5 => Document::Binary((0..rng.below(20)).map(|i| (i * 37) as u8).collect()),
            6 => Document::Id(DocId(
                rng.next().to_le_bytes().repeat(2).try_into().unwrap(),
            )),
            7 => Document::Float(rng.next() as f64 / 3.0),
            8 => Document::Array(
                (0..rng.below(4))
                    .map(|_| random_value(rng, depth - 1))
                    .collect(),
            ),
            _ => {
                // Tag-looking keys on purpose.
                let keys = [
                    "a",
                    "$id",
                    "$object",
                    "$value",
                    "$binary",
                    "_id",
                    "$collection",
                ];
                let n = rng.below(3);
                let fields: Vec<_> = (0..n)
                    .map(|_| (keys[rng.below(keys.len())], random_value(rng, depth - 1)))
                    .collect();
                object(&fields)
            }
        }
    }

    #[test]
    fn export_then_import_reproduces_every_document_id_and_index() {
        let dir = tempfile::tempdir().unwrap();
        let source = open(&dir, "source.trunkdb");
        let mut rng = XorShift(0x5eed);

        let users = source.collection::<Document>("users");
        users.ensure_index("age").unwrap();
        let mut batch = Vec::new();
        for i in 0..2500 {
            // Top-level documents of every kind, not just objects.
            let doc = if i % 10 == 0 {
                random_value(&mut rng, 3)
            } else {
                let mut fields = vec![
                    ("age", Document::Int(i % 90)),
                    ("x", random_value(&mut rng, 3)),
                ];
                if i % 7 == 0 {
                    fields = vec![("$collection", Document::String("not a header".into()))];
                }
                object(&fields)
            };
            batch.push(WriteOp::Insert(
                "users".into(),
                DocId((i as u128).to_be_bytes()),
                doc,
            ));
        }
        source.write_batch(batch).unwrap();
        // Large documents, stored in overflow pages.
        let big = source.collection::<Document>("big");
        big.insert(object(&[("text", Document::String("x".repeat(100_000)))]))
            .unwrap();
        big.ensure_unique_index("text").unwrap();
        // A collection that exists but is empty, and one with only an
        // index, on a nested path.
        source
            .transact(|c, s| get_or_create_meta(c, s, "empty").map(|_| ()))
            .unwrap();
        source
            .collection::<Document>("indexed")
            .ensure_index("k.a")
            .unwrap();
        source
            .collection::<Document>("indexed")
            .ensure_unique_index("u")
            .unwrap();

        let text = export_text(&source);
        let target = open(&dir, "target.trunkdb");
        let summary = import_text(&target, &text).unwrap();
        assert_eq!(
            summary,
            Summary {
                collections: 4,
                documents: 2501
            }
        );
        assert_same_contents(&target, &source);

        // The rebuilt index answers queries, and it all survives a reopen.
        drop(target);
        let target = open(&dir, "target.trunkdb");
        assert_same_contents(&target, &source);
        let filter = Filter {
            conditions: vec![Condition::Compare {
                field: "age".into(),
                op: Op::Eq,
                value: Document::Int(42),
            }],
            ..Filter::default()
        };
        let users = target.collection::<Document>("users");
        assert!(matches!(
            users.explain(&filter).unwrap(),
            crate::query::QueryPlan::Index { .. }
        ));
        assert_eq!(
            users.count(filter.clone()).unwrap(),
            source
                .collection::<Document>("users")
                .count(filter)
                .unwrap()
        );
        // Exporting the copy gives the same text again.
        assert_eq!(export_text(&target), text);
    }

    #[test]
    fn a_hand_written_file_imports() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir, "db.trunkdb");
        let text = r#"{"$trunkdb_export": 1}

{"$collection": "people", "$indexes": ["name", {"field": "born", "unique": true}]}
{"name": "Ada", "born": 1815, "tags": ["math"]}
{"name": "Grace", "born": 1906}
{"$collection": "notes"}
"#;
        let summary = import_text(&db, text).unwrap();
        assert_eq!(
            summary,
            Summary {
                collections: 2,
                documents: 2
            }
        );
        assert_eq!(db.collections().unwrap(), ["notes", "people"]);
        let people = db.collection::<Document>("people");
        assert_eq!(people.indexes().unwrap(), ["name", "born"]);
        assert_eq!(people.unique_indexes().unwrap(), ["born"]);
        let found = people
            .find_one(Filter {
                conditions: vec![Condition::Compare {
                    field: "name".into(),
                    op: Op::Eq,
                    value: Document::String("Grace".into()),
                }],
                ..Filter::default()
            })
            .unwrap()
            .unwrap();
        let Document::Object(map) = found else {
            panic!()
        };
        assert_eq!(map["born"], Document::Int(1906));
        assert!(matches!(map["_id"], Document::Id(_)), "got a new id");
    }

    /// The documents are in before the indexes are built (SPEC §30.3), so
    /// a duplicate for a unique index fails after them, and names both.
    #[test]
    fn an_import_that_breaks_a_unique_index_fails_after_its_documents() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir, "db.trunkdb");
        let text = r#"{"$trunkdb_export": 1}
{"$collection": "people", "$indexes": [{"field": "email", "unique": true}]}
{"email": "a@x"}
{"email": "a@x"}
"#;
        let Err(Error::DuplicateValue { field, .. }) = import_text(&db, text) else {
            panic!("expected DuplicateValue");
        };
        assert_eq!(field, "email");
        let people = db.collection::<Document>("people");
        assert_eq!(people.count(Filter::default()).unwrap(), 2);
        assert!(people.indexes().unwrap().is_empty());
    }

    #[test]
    fn bad_lines_name_their_line_number() {
        let dir = tempfile::tempdir().unwrap();
        let header = "{\"$trunkdb_export\":1}\n";
        let collection = "{\"$collection\":\"c\"}\n";
        let cases: Vec<(String, usize, &str)> = vec![
            (String::new(), 1, "empty input"),
            ("{\"$trunkdb_export\":2}\n".into(), 1, "version 2"),
            ("{\"a\":1}\n".into(), 1, "first line must be"),
            (format!("{header}{{\"a\":1}}\n"), 2, "before the first"),
            (format!("{header}{collection}not json\n"), 3, "not JSON"),
            (
                format!("{header}{collection}[1]\n"),
                3,
                "must be a JSON object",
            ),
            (
                format!("{header}{collection}{{\"_id\":\"x\"}}\n"),
                3,
                "`_id` must be",
            ),
            (
                format!("{header}{collection}{{\"n\":18446744073709551615}}\n"),
                3,
                "i64",
            ),
            (
                format!("{header}{{\"$collection\":1}}\n"),
                2,
                "must be a string",
            ),
            (
                format!("{header}{{\"$collection\":\"c\",\"$index\":[]}}\n"),
                2,
                "unknown key",
            ),
            (
                format!("{header}{{\"$collection\":\"c\",\"$indexes\":[{{\"unique\":true}}]}}\n"),
                2,
                "must be a field name",
            ),
            (
                format!(
                    "{header}{{\"$collection\":\"c\",\"$indexes\":[{{\"field\":\"a\",\"unique\":1}}]}}\n"
                ),
                2,
                "must be a field name",
            ),
        ];
        for (i, (text, line, needle)) in cases.iter().enumerate() {
            let db = open(&dir, &format!("{i}.trunkdb"));
            match import_text(&db, text) {
                Err(Error::Import { line: got, message }) => {
                    assert_eq!(got, *line, "{text:?}: {message}");
                    assert!(message.contains(needle), "{text:?}: {message}");
                }
                other => panic!("{text:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn an_existing_id_stops_the_import_after_the_earlier_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let source = open(&dir, "source.trunkdb");
        let ops = (0..2500u128)
            .map(|i| WriteOp::Insert("c".into(), DocId(i.to_be_bytes()), Document::Int(i as i64)))
            .collect();
        source.write_batch(ops).unwrap();
        let text = export_text(&source);

        let target = open(&dir, "target.trunkdb");
        // Id 1500 is in the second chunk (documents 1000..2000).
        let id = DocId(1500u128.to_be_bytes());
        target
            .write_batch(vec![WriteOp::Insert("c".into(), id, Document::Null)])
            .unwrap();
        let err = import_text(&target, &text).unwrap_err();
        assert!(matches!(err, Error::DuplicateId { .. }), "{err:?}");
        // The first chunk is in, the failed one rolled back entirely.
        assert_eq!(
            target
                .collection::<Document>("c")
                .count(Filter::default())
                .unwrap(),
            1001
        );
    }

    #[test]
    fn export_is_a_snapshot_writers_wait_for() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir, "db.trunkdb");
        let pad = Document::String("x".repeat(500));
        let ops = (0..200u128)
            .map(|i| {
                let doc = object(&[("v", Document::Int(0)), ("pad", pad.clone())]);
                WriteOp::Insert("c".into(), DocId(i.to_be_bytes()), doc)
            })
            .collect();
        db.write_batch(ops).unwrap();

        /// Collects the export. At its first write — `BufWriter` passes
        /// on 8 KB at a time, so that's early in the export — it starts a
        /// thread that updates the *last* document, and gives it time to
        /// finish. If the export didn't hold the read lock, it would then
        /// export the updated document.
        struct Meddler {
            db: Database,
            out: Vec<u8>,
            writer: Option<std::thread::JoinHandle<()>>,
        }
        impl Write for Meddler {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                if self.writer.is_none() {
                    let db = self.db.clone();
                    self.writer = Some(std::thread::spawn(move || {
                        let doc = object(&[("v", Document::Int(1))]);
                        let c = db.collection::<Document>("c");
                        assert!(c.update(&DocId(199u128.to_be_bytes()), doc).unwrap());
                    }));
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                self.out.extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut meddler = Meddler {
            db: db.clone(),
            out: Vec::new(),
            writer: None,
        };
        db.export(&mut meddler).unwrap();
        let text = String::from_utf8(meddler.out).unwrap();
        assert_eq!(text.matches(r#"{"v":0,"#).count(), 200, "the update got in");
        meddler.writer.unwrap().join().unwrap();
        let now = db
            .collection::<Document>("c")
            .get(&DocId(199u128.to_be_bytes()));
        assert!(matches!(now.unwrap().unwrap(), Document::Object(m) if m["v"] == Document::Int(1)));
    }
}
