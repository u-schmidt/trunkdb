//! A differential target for queries (SPEC §105): any bytes as a program
//! of writes and reads on a valid database, every read held against the
//! same filter applied in memory to a model of what is stored.
//!
//! Unlike the other targets, an `Err` isn't a correct answer here: a
//! read never fails, and a write fails only where the model says it must
//! — a duplicate id, a missing one, or a duplicate in a unique index,
//! found with the database's own filters (SPEC §119).

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use trunkdb::query::{Aggregate, Condition, Filter, Op, SortOrder, Update};
use trunkdb::{
    Change, Database, DocId, Document, Error, IndexOptions, Live, LiveQuery, Map, Upserted,
};

/// Steps a program runs at most, and documents it keeps at most.
const MAX_STEPS: usize = 64;
const MAX_DOCS: usize = 400;
/// Snapshots kept at once (SPEC §105.2).
const MAX_SNAPSHOTS: usize = 2;
const COLLECTION: &str = "docs";

/// The indexes a program can make, each plain or sparse, unique or not
/// (SPEC §105.2, §119).
const INDEXES: [&[&str]; 9] = [
    &["a"],
    &["b"],
    &["o.p"],
    &["tags[*]"],
    &["tags[*].k"],
    &["a", "b"],
    &["b", "a"],
    &["a", "o.p", "c"],
    // `aa` before `a`: a name and its prefix in one entry's document
    // (SPEC §102.3).
    &["aa", "a"],
];
/// Paths conditions compare: the documents' fields, an element of
/// `tags`, `_id`, and one nothing has.
const PATHS: [&str; 11] = [
    "a",
    "aa",
    "b",
    "c",
    "o",
    "o.p",
    "tags",
    "tags[*]",
    "tags[*].k",
    "_id",
    "z",
];
/// Paths a read selects (SPEC §106): the conditions' paths, the whole
/// document, a path inside `o.p`.
const SELECTED: [&str; 13] = [
    "a",
    "aa",
    "b",
    "c",
    "o",
    "o.p",
    "o.p.q",
    "tags",
    "tags[*].k",
    "_id",
    "z",
    "",
    "missing.x",
];
/// Paths an aggregate groups by and sums up (SPEC §107): the documents'
/// fields, without `[*]`.
const AGGREGATED: [&str; 7] = ["a", "aa", "b", "c", "o.p", "tags", "z"];
const SORT_KEYS: [&str; 6] = ["a", "b", "o.p", "c", "tags", "_id"];
const OPS: [Op; 7] = [
    Op::Eq,
    Op::Ne,
    Op::Lt,
    Op::Lte,
    Op::Gt,
    Op::Gte,
    Op::Contains,
];

/// The input, read a byte at a time; past its end every read is 0, so
/// any bytes are a program.
pub struct Program<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Program<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Program { bytes, at: 0 }
    }

    fn done(&self) -> bool {
        self.at >= self.bytes.len()
    }

    fn byte(&mut self) -> u8 {
        let byte = self.bytes.get(self.at).copied().unwrap_or(0);
        self.at += 1;
        byte
    }

    /// A number in `0..n`, from one byte (two past 256).
    fn below(&mut self, n: usize) -> usize {
        let raw = match n > 256 {
            true => usize::from(self.byte()) << 8 | usize::from(self.byte()),
            false => usize::from(self.byte()),
        };
        raw % n.max(1)
    }

    /// True one time in `n`.
    fn one_in(&mut self, n: usize) -> bool {
        self.below(n) == 0
    }
}

/// What a run did, for the smoke run's counts (SPEC §105.6).
#[derive(Default, Debug)]
pub struct Ran {
    /// Steps run, by kind (`STEP_NAMES`).
    pub steps: [usize; STEP_NAMES.len()],
    /// The plans `explain_run` reported, by name.
    pub plans: BTreeSet<String>,
    /// Results `explain_run` counted as answered from their index
    /// entries (SPEC §106.5).
    pub answered: u64,
    /// Matches `explain_aggregate` counted as aggregated from their
    /// index entries (SPEC §107.5).
    pub aggregated: u64,
    /// Unique indexes made (SPEC §119).
    pub unique_indexes: u64,
    /// Writes refused as duplicates in a unique index, where the model
    /// said they must be, and unique indexes refused over duplicates.
    pub refused_duplicates: u64,
    /// Documents reported changed by `Database::subscribe` (SPEC §135).
    pub reported: u64,
    /// Conditions made a `Condition::custom` (SPEC §136).
    pub custom: u64,
    /// Events live queries reported (SPEC §138), each as the model says.
    pub live_events: u64,
}

pub const STEP_NAMES: [&str; 15] = [
    "ensure index",
    "drop index",
    "insert",
    "batch",
    "replace",
    "delete",
    "delete_many",
    "update_fields",
    "upsert",
    "checkpoint",
    "compact",
    "reopen",
    "snapshot",
    "read",
    "aggregate",
];

/// Why the model says a batch fails: its first op that does.
#[derive(Debug)]
enum Refusal {
    DuplicateId,
    NotFound,
    /// `id` written as a duplicate of the documents in `colliding`
    /// (SPEC §119).
    Duplicate {
        id: DocId,
        colliding: Vec<DocId>,
    },
}

impl Refusal {
    fn duplicate(id: DocId, colliding: Vec<DocId>) -> Option<Self> {
        (!colliding.is_empty()).then_some(Refusal::Duplicate { id, colliding })
    }
}

/// A document read through a typed collection: only its id (SPEC §99).
#[derive(Serialize, Deserialize)]
struct IdOnly {
    #[serde(rename = "_id")]
    id: DocId,
}

/// A query as the program made it. `Filter` keeps its parts to itself
/// (SPEC §60), and the checks need them: whether it sorts, skips or
/// limits.
#[derive(Clone, Debug)]
struct Query {
    conditions: Vec<Condition>,
    sort: Vec<(&'static str, SortOrder)>,
    skip: usize,
    limit: Option<usize>,
    /// The paths selected (SPEC §106); `None`: whole documents.
    select: Option<Vec<&'static str>>,
}

impl Query {
    fn filter(&self) -> Filter {
        self.matching().skip(self.skip).limit(self.limit)
    }

    /// Its conditions, sort and selection, without skip and limit.
    fn matching(&self) -> Filter {
        let mut filter = self.conditions_only();
        for (key, order) in &self.sort {
            filter = filter.then_by(*key, *order);
        }
        match &self.select {
            Some(paths) => filter.select(paths.iter().copied()),
            None => filter,
        }
    }

    /// The same query handing out whole documents.
    fn unselected(&self) -> Self {
        Query {
            select: None,
            ..self.clone()
        }
    }

    fn conditions_only(&self) -> Filter {
        (self.conditions.iter().cloned()).fold(Filter::new(), Filter::and)
    }

    /// What `find_one` reads: at most the first.
    fn first(&self) -> Filter {
        let limit = self.limit.map_or(1, |l| l.min(1));
        self.matching().skip(self.skip).limit(limit)
    }

    /// Limited to at most `n`: a `delete_many` or `update_fields` of
    /// everything would leave little for the steps after it to read,
    /// and none of the long runs and many leaves a read should meet.
    fn few(mut self, n: usize) -> Self {
        self.limit = Some(self.limit.unwrap_or(n).clamp(1, n));
        self
    }

    /// With a total order where its skip or limit picks among matches:
    /// `delete_many` and `update_fields` act on what `find` returns,
    /// which without one isn't defined.
    fn total(mut self) -> Self {
        let picks = self.skip > 0 || self.limit.is_some();
        if picks && !self.sort.iter().any(|(key, _)| *key == "_id") {
            self.sort.push(("_id", SortOrder::Asc));
        }
        self
    }
}

/// The values documents and conditions take (SPEC §105.3): chosen to
/// share encodings, or to sit side by side in them, as in §104.7; and
/// strings near the key budget, so that a leaf holds a few entries.
fn pool() -> Vec<Document> {
    let big = 1i64 << 60;
    let long = |n: usize, end: &str| Document::from("y".repeat(n) + end);
    let id = |n: u8| Document::Id(DocId([n; 16]));
    let time = |s: i64, n: u32| Document::DateTime(trunkdb::DateTime::from_unix(s, n).unwrap());
    vec![
        Document::Null,
        Document::Bool(false),
        Document::Bool(true),
        Document::Int(-1),
        Document::Int(0),
        Document::Int(3),
        Document::Int(big),
        Document::Int(big + 1),
        Document::Int(big + 300),
        Document::Int(i64::MAX),
        Document::Int(i64::MIN),
        Document::Float(-0.0),
        Document::Float(2.5),
        Document::Float(3.0),
        Document::Float(big as f64),
        Document::Float(f64::INFINITY),
        Document::Float(f64::NEG_INFINITY),
        Document::Float(f64::NAN),
        "".into(),
        "a".into(),
        "a\0".into(),
        "ab".into(),
        "Straße".into(),
        "STRASSE".into(),
        long(340, "1"),
        long(340, "2"),
        long(990, "1"),
        long(990, "2"),
        long(1100, "1"),
        long(1100, "2"),
        id(1),
        id(2),
        time(0, 0),
        time(0, 1),
        time(-5, 0),
        Document::Array(vec![Document::Int(1)]),
        Document::Object(Map::new()),
        Document::Binary(vec![1]),
        // Equal and unequal by their contents (SPEC §124): `[1]` and
        // `[1.0]`, fields in either order, nested, other bytes.
        Document::Array(vec![Document::Float(1.0)]),
        Document::Array(vec![Document::Int(1), Document::Int(2)]),
        Document::Array(vec![Document::Int(2), Document::Int(1)]),
        object([("a", Document::Int(1)), ("b", Document::Int(2))]),
        object([("b", Document::Int(2)), ("a", Document::Int(1))]),
        object([(
            "a",
            object([("x", Document::Array(vec![Document::Int(1)]))]),
        )]),
        object([(
            "a",
            object([("x", Document::Array(vec![Document::Float(1.0)]))]),
        )]),
        Document::Binary(vec![1, 2]),
    ]
}

/// Whether `condition` is or holds a `Condition::custom`.
fn holds_custom(condition: &Condition) -> bool {
    match condition {
        Condition::Custom(_) => true,
        Condition::All(all) | Condition::Any(all) => all.iter().any(holds_custom),
        Condition::Not(inner)
        | Condition::ElemMatch {
            condition: inner, ..
        } => holds_custom(inner),
        _ => false,
    }
}

/// The paths `condition` reads, for a closure that checks it.
fn paths(condition: &Condition, into: &mut Vec<String>) {
    match condition {
        Condition::Compare { field, .. }
        | Condition::Exists { field, .. }
        | Condition::Size { field, .. }
        | Condition::ElemMatch { field, .. } => into.push(field.clone()),
        Condition::All(all) | Condition::Any(all) => all.iter().for_each(|c| paths(c, into)),
        Condition::Not(inner) => paths(inner, into),
        Condition::Custom(custom) => into.extend(custom.fields().iter().cloned()),
        _ => unreachable!("a condition the program doesn't make"),
    }
}

/// The state of a run: the database, its model, what the program made.
struct Run<'p> {
    path: std::path::PathBuf,
    db: Option<Database>,
    /// What the database should hold, by id, each as stored.
    model: BTreeMap<DocId, Document>,
    /// Kept snapshots, each with the model as it was.
    snapshots: Vec<(trunkdb::Snapshot, BTreeMap<DocId, Document>)>,
    /// The menu's indexes made, with their options.
    indexes: BTreeMap<usize, IndexOptions>,
    pool: Vec<Document>,
    next_id: u16,
    /// In eighths, how often a value is a long string: the program
    /// chooses, so that some build indexes of many short leaves.
    long: usize,
    /// Whether long strings come from four values, two common and two
    /// rare: runs of one value over several leaves, with a short run
    /// between, which is where a range's bounds meet whole leaves.
    runs: bool,
    /// A subscription to the database's commits (SPEC §135), made after
    /// the prelude and again at every reopen.
    changes: Option<trunkdb::Changes>,
    /// Live queries (SPEC §138), made with the subscription: each with
    /// its filter and the ids it has reported matching.
    live: Vec<Watched>,
    ran: Ran,
    step: usize,
    /// `QUERIES_TRACE` set: each step said on stderr, for a replay.
    trace: bool,
    program: Program<'p>,
}

/// A live query and what it has said so far.
struct Watched {
    filter: Filter,
    query: LiveQuery<Document>,
    matching: BTreeSet<DocId>,
}

/// Runs the program `input` (SPEC §105.2), panicking where the database
/// answers otherwise than the model says it must.
pub fn run(input: &[u8]) -> Ran {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queries.trunkdb");
    let db = Database::open(&path).unwrap();
    let mut run = Run {
        path,
        db: Some(db),
        model: BTreeMap::new(),
        snapshots: Vec::new(),
        indexes: BTreeMap::new(),
        pool: pool(),
        next_id: 0,
        long: 1,
        runs: false,
        changes: None,
        live: Vec::new(),
        ran: Ran::default(),
        step: 0,
        trace: std::env::var_os("QUERIES_TRACE").is_some(),
        program: Program::new(input),
    };
    run.prelude();
    run.changes = Some(run.db().subscribe().unwrap());
    run.watch();
    while !run.program.done() && run.step < MAX_STEPS {
        run.step();
        run.step += 1;
    }
    run.check();
    let everything = Query {
        conditions: Vec::new(),
        sort: Vec::new(),
        skip: 0,
        limit: None,
        select: None,
    };
    for at in 0..run.snapshots.len() {
        run.read_snapshot(at, &everything);
    }
    run.ran
}

impl Run<'_> {
    fn db(&self) -> &Database {
        self.db.as_ref().expect("open between steps")
    }

    fn docs(&self) -> trunkdb::Collection<Document> {
        self.db().collection(COLLECTION)
    }

    /// A start with data (SPEC §105.3): a few indexes, and 100 to 300
    /// documents in one batch, so that the steps after it meet indexes of
    /// several leaves from the first.
    fn prelude(&mut self) {
        self.long = [1, 1, 4, 7][self.program.below(4)];
        self.runs = self.program.one_in(3);
        for _ in 0..self.program.below(4) {
            self.ensure_index();
        }
        let docs = self.docs();
        let mut batch = self.db().batch();
        for _ in 0..100 + self.program.below(201) {
            let id = self.new_id();
            let doc = Self::with_id(self.document(), id);
            // One a unique index made above would refuse is left out.
            if self.model.contains_key(&id) || !self.collisions(&self.model, id, &doc).is_empty() {
                continue;
            }
            self.model.insert(id, doc.clone());
            batch.insert(&docs, doc).unwrap();
        }
        batch.commit().unwrap();
        self.stored_is_model();
    }

    fn step(&mut self) {
        let kind = self.program.below(STEP_NAMES.len() + 4);
        // Reads and inserts come up more often than the rest.
        let kind = match kind {
            k if k >= STEP_NAMES.len() => [13, 13, 2, 3][k - STEP_NAMES.len()],
            k => k,
        };
        self.ran.steps[kind] += 1;
        if self.trace {
            eprintln!(
                "step {}: {} ({} documents, indexes {:?}, snapshots {})",
                self.step,
                STEP_NAMES[kind],
                self.model.len(),
                self.indexes.keys().map(|&i| INDEXES[i]).collect::<Vec<_>>(),
                self.snapshots.len()
            );
        }
        // The steps that write documents, and the model before them.
        let before = (2..=8).contains(&kind).then(|| self.model.clone());
        match kind {
            0 => self.ensure_index(),
            1 => self.drop_index(),
            2 => self.insert(),
            3 => self.batch(),
            4 => self.replace(),
            5 => self.delete(),
            6 => self.delete_many(),
            7 => self.update_fields(),
            8 => self.upsert(),
            9 => self.db().checkpoint().unwrap(),
            10 => self.compact(),
            11 => self.reopen(),
            12 => self.keep_snapshot(),
            14 => self.aggregate(),
            _ => {
                let query = self.query();
                self.read(&query);
            }
        }
        self.changes_are_reported(kind, before);
        self.live_is_model();
        // Rule 1 in full on one write step in four, and after a
        // compaction or a reopen; on the others, the count and a few
        // documents (SPEC §105.4: reading every document every step was
        // most of a run's time).
        match kind {
            13 => {}
            10 | 11 => self.stored_is_model(),
            _ if self.program.one_in(4) => self.stored_is_model(),
            _ => self.stored_looks_like_model(),
        }
        // Traced, a replay checks the file after every step, to find the
        // first that leaves it wrong.
        if self.program.one_in(16) || self.trace {
            self.check();
        }
    }

    /// What the step says, for a failure's message.
    fn at(&self) -> String {
        format!("step {} of {}", self.step, MAX_STEPS)
    }

    // --- Documents and values (SPEC §105.3) ---

    /// A value: small numbers and short strings that repeat, so that an
    /// `Eq` finds runs; long strings that differ only at their end, so
    /// that an index has many leaves and cut values share their keys; or
    /// one of the pool.
    fn value(&mut self) -> Document {
        if self.program.below(8) < self.long {
            // 400 bytes: exact in a key of one or two fields, cut in one
            // of three; 990: exact in one field; 1100: cut everywhere.
            // Their ends from 8 or 256, for long runs or short ones.
            if self.runs {
                let end = [0, 0, 0, 1, 2, 2, 2, 3][self.program.below(8)];
                return Document::String("y".repeat(990) + &end.to_string());
            }
            let n = [400, 990, 1100][self.program.below(3)];
            let ends = [8, 256][self.program.below(2)];
            let end = self.program.below(ends);
            return Document::String("y".repeat(n) + &end.to_string());
        }
        match self.program.below(7) {
            0 | 1 => Document::Int(self.program.below(8) as i64),
            2 | 3 => Document::String(format!("s{}", self.program.below(8))),
            _ => self.pool[self.program.below(self.pool.len())].clone(),
        }
    }

    /// A value a stored document holds at `path`, now and then; else one
    /// from the pool.
    fn value_for(&mut self, path: &str) -> Document {
        if self.program.below(3) != 0 && !self.model.is_empty() {
            let at = self.program.below(self.model.len());
            let doc = self.model.values().nth(at).expect("in range");
            let mut found = values_at(doc, path);
            if !found.is_empty() {
                let pick = self.program.below(found.len());
                return found.swap_remove(pick);
            }
        }
        if path == "_id" {
            return Document::Id(self.some_id());
        }
        self.value()
    }

    fn new_id(&mut self) -> DocId {
        self.next_id = self.next_id.wrapping_add(1);
        let mut id = [0u8; 16];
        id[0] = self.program.byte();
        id[14..].copy_from_slice(&self.next_id.to_be_bytes());
        DocId(id)
    }

    /// A stored document's id mostly, a new one now and then.
    fn some_id(&mut self) -> DocId {
        match self.model.is_empty() || self.program.one_in(5) {
            true => self.new_id(),
            false => {
                let at = self.program.below(self.model.len());
                *self.model.keys().nth(at).expect("in range")
            }
        }
    }

    fn document(&mut self) -> Map {
        let mut fields = Map::new();
        for name in ["a", "aa", "b", "c"] {
            if !self.program.one_in(6) {
                let value = self.value();
                fields.insert(name.to_string(), value);
            }
        }
        match self.program.below(5) {
            0 => {}
            1 => {
                let value = self.value();
                fields.insert("o".into(), value);
            }
            _ => {
                let p = self.value();
                fields.insert("o".into(), object([("p", p)]));
            }
        }
        match self.program.below(8) {
            0 => {}
            1 => {
                let value = self.value();
                fields.insert("tags".into(), value);
            }
            _ => {
                let tags = (0..self.program.below(5))
                    .map(|_| match self.program.one_in(3) {
                        true => {
                            let k = self.value();
                            object([("k", k)])
                        }
                        false => self.value(),
                    })
                    .collect();
                fields.insert("tags".into(), Document::Array(tags));
            }
        }
        if self.program.one_in(12) {
            fields.insert("blob".into(), Document::from("z".repeat(5000)));
        }
        fields
    }

    /// A document with `id` as its `_id`, as an insert stores it.
    fn with_id(fields: Map, id: DocId) -> Document {
        let mut doc = Map::new();
        doc.insert("_id".to_string(), Document::Id(id));
        doc.extend(fields);
        Document::Object(doc)
    }

    // --- Queries (SPEC §105.3) ---

    fn condition(&mut self, depth: usize) -> Condition {
        if depth == 0 || self.program.below(3) != 0 {
            let leaf = self.leaf();
            return self.maybe_custom(leaf);
        }
        let children: Vec<Condition> = (0..self.program.below(4))
            .map(|_| self.condition(depth - 1))
            .collect();
        match self.program.below(3) {
            0 => Condition::all(children),
            1 => Condition::any(children),
            _ => !self.condition(depth - 1),
        }
    }

    /// One time in six, `condition` made a closure that checks it
    /// (SPEC §136), naming the paths it reads: in every plan it must
    /// answer as the condition does, so the model, which checks the
    /// closure as it checks the condition, says what it finds.
    fn maybe_custom(&mut self, condition: Condition) -> Condition {
        if !self.program.one_in(6) {
            return condition;
        }
        self.ran.custom += 1;
        let mut fields = Vec::new();
        paths(&condition, &mut fields);
        Condition::custom(fields, move |doc| condition.matches(doc))
    }

    fn leaf(&mut self) -> Condition {
        {
            match self.program.below(11) {
                0 => Condition::exists(PATHS[self.program.below(PATHS.len())]),
                1 => Condition::missing(PATHS[self.program.below(PATHS.len())]),
                2 => {
                    let op = OPS[self.program.below(6)].clone();
                    Condition::size("tags", op, self.program.below(5))
                }
                // An OR of `eq`s on two paths, which a union of index
                // ranges reads where both are indexed (SPEC §36.3).
                4 => {
                    let (a, b) = (self.program.below(5), self.program.below(5));
                    let paths = ["a", "b", "o.p", "tags[*]", "tags[*].k"];
                    let (x, y) = (self.value_for(paths[a]), self.value_for(paths[b]));
                    Condition::eq(paths[a], x) | Condition::eq(paths[b], y)
                }
                // `in` (SPEC §115): an OR of `eq`s on one path, a point
                // of its index per value; none, nothing.
                5 => {
                    let paths = ["a", "b", "o.p", "tags[*]", "_id"];
                    let path = paths[self.program.below(paths.len())];
                    let values: Vec<Document> = (0..self.program.below(4))
                        .map(|_| match path {
                            "_id" => Document::Id(self.some_id()),
                            _ => self.value_for(path),
                        })
                        .collect();
                    Condition::is_in(path, values)
                }
                3 => {
                    let inner = match self.program.one_in(2) {
                        true => "",
                        false => "k",
                    };
                    let op = OPS[self.program.below(OPS.len())].clone();
                    let value = self.value_for("tags[*]");
                    Condition::elem_match("tags", Condition::compare(inner, op, value))
                }
                _ => {
                    let path = PATHS[self.program.below(PATHS.len())];
                    let op = OPS[self.program.below(OPS.len())].clone();
                    let value = self.value_for(path);
                    Condition::compare(path, op, value)
                }
            }
        }
    }

    fn query(&mut self) -> Query {
        if !self.indexes.is_empty() && self.program.one_in(2) {
            return self.focused();
        }
        let conditions = (0..self.program.below(4))
            .map(|_| self.condition(3))
            .collect();
        let sort = (0..self.program.below(4))
            .map(|_| {
                let key = SORT_KEYS[self.program.below(SORT_KEYS.len())];
                (
                    key,
                    [SortOrder::Asc, SortOrder::Desc][self.program.below(2)],
                )
            })
            .collect();
        let n = self.model.len();
        let skip = match self.program.below(4) {
            0 | 1 => 0,
            2 => self.program.below(4),
            _ => self.program.below(n + 2),
        };
        let limit = match self.program.below(6) {
            0 | 1 => None,
            2 => Some(0),
            3 => Some(1),
            4 => Some(1 + self.program.below(20)),
            _ => Some(n + 1 + self.program.below(4)),
        };
        let select = self.selection(None);
        Query {
            conditions,
            sort,
            skip,
            limit,
            select,
        }
    }

    /// A query on one existing index's fields (SPEC §105.3): `eq`s fixing
    /// its first fields, a range on the next, now and then `ne`,
    /// `contains`, an OR or a NOT on its fields and an `_id` bound;
    /// sorted by `_id`, by the index's fields or anyhow. These are the
    /// queries whose entries decide (§100, §104), whose leaves are passed
    /// whole (§101) and which walk an `Eq`-fixed range by id (§103).
    fn focused(&mut self) -> Query {
        let at = self.program.below(self.indexes.len());
        let index = *self.indexes.keys().nth(at).expect("in range");
        let fields = INDEXES[index];
        let fixed = self.program.below(fields.len() + 1);
        let mut conditions = Vec::new();
        for field in &fields[..fixed] {
            let value = self.value_for(field);
            conditions.push(Condition::eq(*field, value));
        }
        // Ranges on the field after the fixed ones, and now and then on
        // the one after that too, which the first range leaves unordered.
        let ranged = 1 + usize::from(self.program.one_in(2));
        for next in fields.iter().skip(fixed).take(ranged) {
            // Strictly between two stored values, now and then: where a
            // range's bounds meet runs of their own values (SPEC §104.4).
            if !next.contains(['.', '[']) && self.program.one_in(3) {
                let (x, y) = (self.value_for(next), self.value_for(next));
                let y_above = Condition::gt(*next, x.clone()).matches(&object([(next, y.clone())]));
                let (low, high) = if y_above { (x, y) } else { (y, x) };
                conditions.push(Condition::gt(*next, low));
                conditions.push(Condition::lt(*next, high));
                continue;
            }
            for _ in 0..self.program.below(3) {
                let op = [Op::Gt, Op::Gte, Op::Lt, Op::Lte][self.program.below(4)].clone();
                let value = self.value_for(next);
                conditions.push(Condition::compare(*next, op, value));
            }
        }
        let field = fields[self.program.below(fields.len())];
        match self.program.below(6) {
            0 => {
                let value = self.value_for(field);
                conditions.push(Condition::ne(field, value));
            }
            1 => {
                let needle = match self.value_for(field) {
                    Document::String(s) => s.chars().skip(self.program.below(3)).take(4).collect(),
                    _ => "s".to_string(),
                };
                conditions.push(Condition::contains(field, needle));
            }
            2 => {
                let (x, y) = (self.value_for(field), self.value_for(field));
                conditions.push(Condition::eq(field, x) | Condition::lt(field, y));
            }
            3 => {
                let value = self.value_for(field);
                conditions.push(!Condition::gte(field, value));
            }
            _ => {}
        }
        if self.program.one_in(4) {
            let op = [Op::Gt, Op::Gte, Op::Lt, Op::Lte, Op::Ne][self.program.below(5)].clone();
            let id = Document::Id(self.some_id());
            conditions.push(Condition::compare("_id", op, id));
        }
        let order = [SortOrder::Asc, SortOrder::Desc][self.program.below(2)];
        let sort = match self.program.below(4) {
            0 => Vec::new(),
            1 => vec![("_id", order)],
            2 => fields
                .iter()
                .filter(|f| !f.contains("[*]"))
                .map(|f| (*f, order))
                .collect(),
            _ => vec![(SORT_KEYS[self.program.below(SORT_KEYS.len())], order)],
        };
        let n = self.model.len();
        let skip = match self.program.below(3) {
            0 => 0,
            1 => self.program.below(8),
            _ => self.program.below(n + 2),
        };
        let limit = match self.program.below(4) {
            0 => None,
            _ => Some(1 + self.program.below(30)),
        };
        let select = self.selection(Some(fields));
        Query {
            conditions,
            sort,
            skip,
            limit,
            select,
        }
    }

    /// What a read selects, half the time (SPEC §106): some of `index`'s
    /// fields, whose entries may answer it (§106.5), or any of
    /// `SELECTED`.
    fn selection(&mut self, index: Option<&'static [&'static str]>) -> Option<Vec<&'static str>> {
        match (self.program.below(4), index) {
            (0 | 1, _) => None,
            (2, Some(fields)) => Some(fields[..1 + self.program.below(fields.len())].to_vec()),
            _ => Some(
                (0..self.program.below(4))
                    .map(|_| SELECTED[self.program.below(SELECTED.len())])
                    .collect(),
            ),
        }
    }

    /// Rule 2 for an aggregate (SPEC §107): `aggregate` answers as
    /// `Aggregate::apply` does on the model's matches. Half the time its
    /// filter neither sorts nor limits and its fields are an existing
    /// index's, so that the entries may answer (§107.5); half its columns
    /// have a condition (§126).
    fn aggregate(&mut self) {
        // A skip or a limit picks among matches only in a total order.
        let mut query = self.query().unselected().total();
        if self.program.one_in(2) {
            query.sort.clear();
            query.skip = 0;
            query.limit = None;
        }
        let index_fields: Vec<&'static str> = match self.indexes.is_empty() {
            false if self.program.one_in(2) => {
                let at = self.program.below(self.indexes.len());
                let index = *self.indexes.keys().nth(at).expect("in range");
                INDEXES[index]
                    .iter()
                    .copied()
                    .filter(|f| !f.contains('['))
                    .collect()
            }
            _ => Vec::new(),
        };
        let fields: Vec<&'static str> = match index_fields.is_empty() {
            true => AGGREGATED.to_vec(),
            false => index_fields,
        };
        let mut aggregate = Aggregate::new();
        let mut grouped = Vec::new();
        for _ in 0..self.program.below(3) {
            let field = fields[self.program.below(fields.len())];
            if !grouped.contains(&field) {
                grouped.push(field);
                aggregate = aggregate.group_by(field);
            }
        }
        for c in 0..1 + self.program.below(3) {
            let name = format!("c{c}");
            let path = fields[self.program.below(fields.len())];
            let mut column = match self.program.below(5) {
                0 => aggregate.count(),
                1 => aggregate.sum(path),
                2 => aggregate.min(path),
                3 => aggregate.max(path),
                _ => aggregate.avg(path),
            };
            // A condition on the column (SPEC §126), as a filter's
            // conditions are made.
            if self.program.one_in(2) {
                column = column.filter(self.condition(1));
            }
            aggregate = column.named(name);
        }
        if self.program.one_in(4) {
            aggregate = aggregate.sort_desc("c0").limit(1 + self.program.below(3));
        }
        let filter = query.filter();
        let matches = self.expected(&self.model, &filter);
        let expected = aggregate.apply(matches).unwrap();
        let docs = self.docs();
        let found = docs.aggregate(filter.clone(), &aggregate).unwrap();
        let agree =
            found.len() == expected.len() && found.iter().zip(&expected).all(|(a, b)| same(a, b));
        if !agree {
            let text = |rows: &[Document]| rows.iter().map(canonical).collect::<Vec<_>>();
            assert_eq!(
                text(&found),
                text(&expected),
                "{}: aggregate {filter:?} {aggregate:?}",
                self.at()
            );
        }
        let run = docs.explain_aggregate(filter.clone(), &aggregate).unwrap();
        assert_eq!(
            run.stats.documents_returned as usize,
            expected.len(),
            "{}",
            self.at()
        );
        self.ran.aggregated += run.stats.from_entries;
    }

    // --- Index steps ---

    fn ensure_index(&mut self) {
        let at = self.program.below(INDEXES.len());
        if self.indexes.contains_key(&at) {
            return;
        }
        let mut options = IndexOptions::new();
        if self.program.one_in(3) {
            options = options.sparse();
        }
        if self.program.one_in(3) {
            options = options.unique();
        }
        let made = self.docs().ensure_index_with(INDEXES[at], options.clone());
        // Over documents that hold a duplicate, a unique index is refused
        // and nothing is made (SPEC §33.2).
        let duplicates = options.is_unique()
            && self
                .model
                .iter()
                .any(|(id, doc)| !Self::collisions_in(at, &self.model, *id, doc).is_empty());
        if duplicates {
            let named = match &made {
                Err(Error::DuplicateValue { id, existing, .. }) => Some((*id, *existing)),
                _ => None,
            };
            assert!(
                named.is_some_and(|(id, existing)| self.model.get(&id).is_some_and(|doc| {
                    Self::collisions_in(at, &self.model, id, doc).contains(&existing)
                })),
                "{}: unique {:?} over duplicates: {made:?}",
                self.at(),
                INDEXES[at]
            );
            let fields = |info: &trunkdb::IndexInfo| {
                info.fields()
                    .iter()
                    .map(String::as_str)
                    .eq(INDEXES[at].iter().copied())
            };
            assert!(
                !self.docs().indexes().unwrap().iter().any(fields),
                "{}: a refused unique {:?} was made",
                self.at(),
                INDEXES[at]
            );
            self.ran.refused_duplicates += 1;
            return;
        }
        assert!(
            made.unwrap(),
            "{}: ensure_index {:?}",
            self.at(),
            INDEXES[at]
        );
        self.ran.unique_indexes += u64::from(options.is_unique());
        self.indexes.insert(at, options);
    }

    fn drop_index(&mut self) {
        let Some(&at) = self.indexes.keys().nth(self.program.below(INDEXES.len())) else {
            return;
        };
        assert!(
            self.docs().drop_index(INDEXES[at]).unwrap(),
            "{}",
            self.at()
        );
        self.indexes.remove(&at);
    }

    // --- Writes (SPEC §105.1) ---

    fn room(&self) -> bool {
        self.model.len() < MAX_DOCS
    }

    fn insert(&mut self) {
        let id = self.some_id();
        let doc = Self::with_id(self.document(), id);
        if !self.room() && !self.model.contains_key(&id) {
            return;
        }
        let result = self.docs().insert(doc.clone());
        if self.model.contains_key(&id) {
            assert!(
                matches!(result, Err(Error::DuplicateId { .. })),
                "{}: insert of a stored id: {result:?}",
                self.at()
            );
            return;
        }
        let colliding = self.collisions(&self.model, id, &doc);
        if self.refused(&result, id, &doc, &colliding, "insert") {
            return;
        }
        assert_eq!(result.unwrap(), id, "{}", self.at());
        self.model.insert(id, doc);
    }

    fn batch(&mut self) {
        let docs = self.docs();
        let mut batch = self.db().batch();
        let mut after = self.model.clone();
        // The first op that fails, which fails the batch (SPEC §17.3):
        // each is checked against the state the ops before it left
        // (§33.2), so the model goes op by op as well.
        let mut fails: Option<Refusal> = None;
        for _ in 0..1 + self.program.below(32) {
            let id = self.some_id();
            // Half inserts, so that collections grow as much as they
            // shrink, with deletes and the rest of the steps.
            let refusal = match self.program.below(4) {
                0 | 1 if after.len() < MAX_DOCS => {
                    let doc = Self::with_id(self.document(), id);
                    let refusal = match after.contains_key(&id) {
                        true => Some(Refusal::DuplicateId),
                        false => Refusal::duplicate(id, self.collisions(&after, id, &doc)),
                    };
                    after.insert(id, doc.clone());
                    batch.insert(&docs, doc).unwrap();
                    refusal
                }
                2 => {
                    let doc = Self::with_id(self.document(), id);
                    let refusal = match after.contains_key(&id) {
                        false => Some(Refusal::NotFound),
                        true => Refusal::duplicate(id, self.collisions(&after, id, &doc)),
                    };
                    after.insert(id, doc.clone());
                    batch.update(&docs, &id, doc).unwrap();
                    refusal
                }
                _ => {
                    batch.delete(&docs, &id);
                    after.remove(&id).is_none().then_some(Refusal::NotFound)
                }
            };
            if self.trace {
                eprintln!("  batch op on {id}: the model says {refusal:?}");
            }
            fails = fails.or(refusal);
        }
        let result = batch.commit();
        let as_said = match (&fails, &result) {
            (None, Ok(())) => {
                self.model = after;
                return;
            }
            (Some(Refusal::DuplicateId), Err(Error::DuplicateId { .. })) => true,
            (Some(Refusal::NotFound), Err(Error::NotFound { .. })) => true,
            (
                Some(Refusal::Duplicate { id, colliding }),
                Err(Error::DuplicateValue {
                    id: i, existing, ..
                }),
            ) => {
                self.ran.refused_duplicates += 1;
                i == id && colliding.contains(existing)
            }
            _ => false,
        };
        assert!(
            as_said,
            "{}: a batch the model says {fails:?} of: {result:?}",
            self.at()
        );
    }

    fn replace(&mut self) {
        let id = self.some_id();
        let fields = self.document();
        let doc = Self::with_id(fields, id);
        let result = self.docs().update(&id, doc.clone());
        if self.model.contains_key(&id) {
            let colliding = self.collisions(&self.model, id, &doc);
            if self.refused(&result, id, &doc, &colliding, "update") {
                return;
            }
        }
        let replaced = result.unwrap();
        assert_eq!(replaced, self.model.contains_key(&id), "{}", self.at());
        if replaced {
            self.model.insert(id, doc);
        }
    }

    fn delete(&mut self) {
        let id = self.some_id();
        let deleted = self.docs().delete(&id).unwrap();
        assert_eq!(deleted, self.model.remove(&id).is_some(), "{}", self.at());
    }

    fn delete_many(&mut self) {
        let filter = self.query().few(10).total().filter();
        let doomed = self.expected(&self.model, &filter);
        let deleted = self.docs().delete_many(filter.clone()).unwrap();
        assert_eq!(
            deleted,
            doomed.len(),
            "{}: delete_many {filter:?}",
            self.at()
        );
        for doc in doomed {
            self.model.remove(&id_of(&doc));
        }
    }

    fn update_fields(&mut self) {
        let filter = self.query().few(20).total().filter();
        let mut update = Update::new();
        let fields = ["a", "b", "c", "o.p", "tags", "z"];
        for _ in 0..1 + self.program.below(3) {
            let path = fields[self.program.below(fields.len())];
            let value = self.value();
            update = match self.program.below(9) {
                0 => update.set(path, value),
                1 => update.unset(path),
                2 => update.inc(path, Document::Int(self.program.below(3) as i64)),
                3 => update.min(path, value),
                4 => update.max(path, value),
                5 => update.push(path, value),
                6 => update.pull(path, value),
                7 => update.add_to_set(path, value),
                _ => update.rename(path, fields[self.program.below(fields.len())]),
            };
        }
        let touched: Vec<DocId> = (self.expected(&self.model, &filter).iter())
            .map(id_of)
            .collect();
        if self.trace {
            eprintln!("  update_fields {filter:?} {update:?}, touching {touched:?}");
        }
        match self.docs().update_fields(filter.clone(), &update) {
            Ok(changed) => {
                assert!(
                    changed <= touched.len(),
                    "{}: {changed} of {}",
                    self.at(),
                    touched.len()
                );
                // The model doesn't apply operators: the touched documents
                // come back as stored, and the others must stay as they
                // are (SPEC §105.1).
                for id in touched {
                    let doc = self.docs().get(&id).unwrap();
                    self.model
                        .insert(id, doc.expect("an updated document stays"));
                }
                // Nor does it know which update would make a duplicate,
                // so whatever was let through mustn't hold one (SPEC
                // §119).
                for (id, doc) in &self.model {
                    let colliding = self.collisions(&self.model, *id, doc);
                    assert!(
                        colliding.is_empty(),
                        "{}: update_fields {filter:?} {update:?} left {id} a duplicate of {colliding:?}",
                        self.at()
                    );
                }
            }
            // Refused: a document it changes, colliding with one stored.
            Err(Error::DuplicateValue { id, existing, .. }) => {
                assert!(
                    touched.contains(&id) && self.model.contains_key(&existing),
                    "{}: update_fields {filter:?} {update:?} refused {id} for {existing}",
                    self.at()
                );
                self.ran.refused_duplicates += 1;
            }
            Err(Error::Update { .. }) => {}
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::InvalidInput => {}
            Err(e) => panic!("{}: update_fields {filter:?} {update:?}: {e:?}", self.at()),
        }
    }

    fn upsert(&mut self) {
        let query = self.query();
        let filter = query.filter();
        let id = self.new_id();
        if !self.room() {
            return;
        }
        let doc = Self::with_id(self.document(), id);
        // Sort, skip and limit don't matter to an upsert.
        let conditions = query.conditions_only();
        let matches: Vec<DocId> = (self.expected(&self.model, &conditions).iter())
            .map(id_of)
            .collect();
        let result = self.docs().upsert(filter.clone(), doc.clone());
        // An upsert replaces its one match, keeping its id, or inserts.
        let into = match matches[..] {
            [one] => Some(one),
            [] => Some(id),
            _ => None,
        };
        if let Some(into) = into {
            let colliding = self.collisions(&self.model, into, &doc);
            if self.refused(&result, into, &doc, &colliding, "upsert") {
                return;
            }
        }
        match matches[..] {
            [] => {
                assert!(
                    matches!(result, Ok(Upserted::Inserted(i)) if i == id),
                    "{}: upsert {filter:?}: {result:?}",
                    self.at()
                );
                self.model.insert(id, doc);
            }
            [one] => {
                assert!(
                    matches!(result, Ok(Upserted::Updated(i)) if i == one),
                    "{}: upsert {filter:?}: {result:?}",
                    self.at()
                );
                let stored = self.docs().get(&one).unwrap().expect("upserted");
                self.model.insert(one, stored);
            }
            _ => assert!(
                matches!(result, Err(Error::MultipleMatches { .. })),
                "{}: upsert {filter:?}: {result:?}",
                self.at()
            ),
        }
    }

    // --- Unique indexes (SPEC §119) ---

    /// The documents of `model`, other than `id`, that `doc` would be a
    /// duplicate of in the unique index `at`. No rule of its own: §33.1
    /// says a unique index holds exactly what `find(field == v)` never
    /// finds twice, so each value `doc` holds there (each element, for
    /// `[*]`) is looked for with the database's own `Eq`, the AND of
    /// them for a compound index. A null — or a field missing, which a
    /// filter can't tell from null — collides with nothing; nor do
    /// arrays, objects and binary, which `Eq` compares by content but a
    /// unique index doesn't check (SPEC §124.5); `NaN` equals nothing.
    fn collisions_in(
        at: usize,
        model: &BTreeMap<DocId, Document>,
        id: DocId,
        doc: &Document,
    ) -> Vec<DocId> {
        let fields = INDEXES[at];
        let tuples: Vec<Vec<Document>> = match fields {
            [one] => values_at(doc, one).into_iter().map(|v| vec![v]).collect(),
            many => vec![
                many.iter()
                    .map(|field| {
                        values_at(doc, field)
                            .into_iter()
                            .next()
                            .unwrap_or(Document::Null)
                    })
                    .collect(),
            ],
        };
        let mut colliding = Vec::new();
        for tuple in tuples {
            if tuple.iter().any(|value| {
                matches!(
                    value,
                    Document::Null | Document::Array(_) | Document::Object(_) | Document::Binary(_)
                )
            }) {
                continue;
            }
            let filter = (fields.iter().zip(tuple))
                .fold(Filter::new(), |f, (field, value)| f.eq(*field, value));
            let others = model.values().filter(|other| id_of(other) != id).cloned();
            colliding.extend(filter.apply(others).iter().map(id_of));
        }
        colliding
    }

    /// Where `doc` and the stored `other` collide, for a failure's
    /// message: each unique index, its fields' values in both.
    fn collision_shown(&self, doc: &Document, other: DocId) -> String {
        let other = self.model.get(&other).cloned().unwrap_or(Document::Null);
        (self.indexes.iter())
            .filter(|(_, options)| options.is_unique())
            .map(|(&at, options)| {
                let values = |d: &Document| -> Vec<Vec<Document>> {
                    INDEXES[at].iter().map(|f| values_at(d, f)).collect()
                };
                format!(
                    "{:?} ({options:?}): {:?} against {:?}",
                    INDEXES[at],
                    values(doc),
                    values(&other)
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// `collisions_in` every unique index made.
    fn collisions(
        &self,
        model: &BTreeMap<DocId, Document>,
        id: DocId,
        doc: &Document,
    ) -> Vec<DocId> {
        (self.indexes.iter())
            .filter(|(_, options)| options.is_unique())
            .flat_map(|(&at, _)| Self::collisions_in(at, model, id, doc))
            .collect()
    }

    /// Whether a write of `id` the model says is a duplicate of
    /// `colliding` was refused as one, naming one of them; it must be,
    /// and one that isn't mustn't be.
    fn refused<T: std::fmt::Debug>(
        &mut self,
        result: &Result<T, Error>,
        id: DocId,
        doc: &Document,
        colliding: &[DocId],
        what: &str,
    ) -> bool {
        match (colliding.is_empty(), result) {
            (true, Err(Error::DuplicateValue { .. })) => {
                panic!(
                    "{}: {what} of {id} refused as a duplicate: {result:?}",
                    self.at()
                )
            }
            (true, _) => false,
            (
                false,
                Err(Error::DuplicateValue {
                    id: i, existing, ..
                }),
            ) if *i == id && colliding.contains(existing) => {
                self.ran.refused_duplicates += 1;
                true
            }
            (false, _) => panic!(
                "{}: {what} of {id}, a duplicate of {colliding:?}: {result:?}; {}",
                self.at(),
                self.collision_shown(doc, colliding[0])
            ),
        }
    }

    // --- Upkeep ---

    fn compact(&mut self) {
        let result = self.db().compact();
        if self.trace {
            eprintln!("  compact: {result:?}");
        }
        match self.snapshots.is_empty() {
            true => {
                result.unwrap();
                self.check();
            }
            // Refused, unless the file is as small as it would get, and
            // nothing is rewritten.
            false => assert!(
                matches!(&result, Err(Error::SnapshotOpen))
                    || matches!(&result, Ok(c) if c.pages_after == c.pages_before),
                "{}: compact beside a snapshot: {result:?}",
                self.at()
            ),
        }
    }

    fn reopen(&mut self) {
        self.snapshots.clear();
        self.db = None;
        self.db = Some(Database::open(&self.path).unwrap());
        // The old subscription ended with the database it was on.
        assert_eq!(self.changes.take().and_then(|old| old.try_recv()), None);
        self.changes = Some(self.db().subscribe().unwrap());
        // The old live queries ended with it too.
        for old in std::mem::take(&mut self.live) {
            assert!(old.query.try_recv().is_none(), "{}", self.at());
        }
        self.watch();
        self.check();
    }

    /// One or two live queries of the program's conditions (SPEC §138):
    /// first an `Entered` for every document the model has matching.
    fn watch(&mut self) {
        for _ in 0..1 + self.program.below(2) {
            let filter = self.query().conditions_only();
            let query = self.docs().watch(filter.clone()).unwrap();
            self.live.push(Watched {
                filter,
                query,
                matching: BTreeSet::new(),
            });
        }
        self.live_is_model();
    }

    /// What each live query reported for the step, a commit at most:
    /// every event names a document as the model has it now, `Entered`
    /// one it didn't report matching and that matches, `Left` one it did
    /// that no longer does or is gone, `Changed` one it did that still
    /// does. After them, it reports matching exactly what matches in the
    /// model.
    fn live_is_model(&mut self) {
        let at = self.at();
        for watched in &mut self.live {
            while let Some(event) = watched.query.try_recv() {
                let event = event.unwrap_or_else(|e| panic!("{at}: {e}"));
                self.ran.live_events += 1;
                let id = event.id();
                let now = self.model.get(&id);
                let is_now = |doc: &Document| {
                    assert!(
                        now.is_some_and(|model| same(model, doc)),
                        "{at}: {id} reported as {doc:?}, the model has {now:?}"
                    );
                    watched.filter.matches(doc)
                };
                match event {
                    Live::Entered { doc, .. } => {
                        assert!(is_now(&doc), "{at}: {id} entered, not matching");
                        assert!(watched.matching.insert(id), "{at}: {id} entered twice");
                    }
                    Live::Left { doc, .. } => {
                        match doc {
                            Some(doc) => assert!(!is_now(&doc), "{at}: {id} left, matching"),
                            None => assert!(now.is_none(), "{at}: {id} left as gone, isn't"),
                        }
                        assert!(watched.matching.remove(&id), "{at}: {id} left, never in");
                    }
                    Live::Changed { doc, .. } => {
                        assert!(is_now(&doc), "{at}: {id} changed, not matching");
                        assert!(watched.matching.contains(&id), "{at}: {id} changed, not in");
                    }
                    other => panic!("{at}: {other:?}"),
                }
            }
            let expected: BTreeSet<DocId> = self
                .model
                .iter()
                .filter(|(_, doc)| watched.filter.matches(doc))
                .map(|(id, _)| *id)
                .collect();
            assert_eq!(
                watched.matching, expected,
                "{at}: live query {:?}",
                watched.filter
            );
        }
    }

    /// What the subscription received for the step (SPEC §135): at most
    /// one commit, from a step that writes documents, which names
    /// exactly the documents the model gained and lost, and as updated
    /// only some it kept, among them every one whose contents changed.
    fn changes_are_reported(&mut self, kind: usize, before: Option<BTreeMap<DocId, Document>>) {
        let changes = self.changes.as_ref().expect("subscribed");
        let commits: Vec<_> = std::iter::from_fn(|| changes.try_recv()).collect();
        let Some(before) = before else {
            assert!(
                commits.is_empty(),
                "{}: {} reported {commits:?}",
                self.at(),
                STEP_NAMES[kind]
            );
            return;
        };
        assert!(commits.len() <= 1, "{}: {commits:?}", self.at());
        let (mut inserted, mut updated, mut deleted) =
            (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
        for change in commits.iter().flat_map(|commit| &commit.changes) {
            assert_eq!(change.collection(), COLLECTION, "{}", self.at());
            let (set, id) = match change {
                Change::Inserted { id, .. } => (&mut inserted, id),
                Change::Updated { id, .. } => (&mut updated, id),
                Change::Deleted { id, .. } => (&mut deleted, id),
                other => panic!("{}: {other:?}", self.at()),
            };
            assert!(set.insert(*id), "{}: {id} twice: {commits:?}", self.at());
            self.ran.reported += 1;
        }
        let gained: BTreeSet<DocId> = self
            .model
            .keys()
            .filter(|id| !before.contains_key(id))
            .copied()
            .collect();
        let lost: BTreeSet<DocId> = before
            .keys()
            .filter(|id| !self.model.contains_key(id))
            .copied()
            .collect();
        let changed: BTreeSet<DocId> = self
            .model
            .iter()
            .filter(|(id, doc)| before.get(id).is_some_and(|was| !same(was, doc)))
            .map(|(id, _)| *id)
            .collect();
        let at = self.at();
        assert_eq!(inserted, gained, "{at}: inserted");
        assert_eq!(deleted, lost, "{at}: deleted");
        assert!(
            changed.is_subset(&updated),
            "{at}: changed {changed:?}, updated {updated:?}"
        );
        assert!(
            updated
                .iter()
                .all(|id| before.contains_key(id) && self.model.contains_key(id)),
            "{at}: updated {updated:?}"
        );
    }

    fn keep_snapshot(&mut self) {
        if self.snapshots.len() == MAX_SNAPSHOTS {
            self.snapshots.remove(0);
        }
        let snapshot = self.db().snapshot().unwrap();
        self.snapshots.push((snapshot, self.model.clone()));
    }

    fn check(&self) {
        let report = self.db().check().unwrap();
        assert!(
            report.is_ok(),
            "{}: check: {:#?}",
            self.at(),
            report.problems
        );
    }

    // --- The oracle (SPEC §105.1) ---

    /// `filter` applied to `model`, in id order: what a read must answer.
    fn expected(&self, model: &BTreeMap<DocId, Document>, filter: &Filter) -> Vec<Document> {
        filter.apply(model.values().cloned())
    }

    /// Rule 1: a plain read of every document gives the model.
    fn stored_is_model(&self) {
        let mut stored = self.docs().find(Filter::default()).unwrap();
        stored.sort_by_key(id_of);
        let agree = stored.len() == self.model.len()
            && stored
                .iter()
                .zip(self.model.values())
                .all(|(a, b)| same(a, b));
        if !agree {
            let stored: Vec<String> = stored.iter().map(canonical).collect();
            let model: Vec<String> = self.model.values().map(canonical).collect();
            let only = |a: &[String], b: &[String]| -> Vec<String> {
                a.iter()
                    .filter(|d| !b.contains(d))
                    .take(3)
                    .cloned()
                    .collect()
            };
            panic!(
                "{}: stored {} documents, the model {}; only stored: {:#?}; only in the model: {:#?}",
                self.at(),
                stored.len(),
                model.len(),
                only(&stored, &model),
                only(&model, &stored),
            );
        }
    }

    /// Rule 1, cheaply: as many documents, and a few of them as the
    /// model has them.
    fn stored_looks_like_model(&mut self) {
        let docs = self.docs();
        let count = docs.count(Filter::new()).unwrap();
        assert_eq!(count, self.model.len(), "{}: stored count", self.at());
        for _ in 0..4.min(self.model.len()) {
            let at = self.program.below(self.model.len());
            let (id, doc) = self.model.iter().nth(at).expect("in range");
            let stored = docs.get(id).unwrap();
            let agree = stored.as_ref().is_some_and(|s| same(s, doc));
            assert!(
                agree,
                "{}: stored {:?}, the model {}",
                self.at(),
                stored.as_ref().map(canonical),
                canonical(doc)
            );
        }
    }

    /// Rule 2: every way of reading answers as the model does.
    fn read(&mut self, query: &Query) {
        let filter = &query.filter();
        let docs = self.docs();
        let typed = self.db().collection::<IdOnly>(COLLECTION);
        let expected = self.expected(&self.model, filter);
        let explained = docs.explain_run(filter.clone()).unwrap();
        self.ran.plans.insert(plan_name(&explained.plan));
        self.ran.answered += explained.stats.from_entries;
        if query.select.is_some() {
            // A selection reads the documents the same filter does, less
            // those answered from their entries (SPEC §106.5).
            let whole = docs.explain_run(query.unselected().filter()).unwrap();
            assert_eq!(
                explained.stats.documents_examined + explained.stats.from_entries,
                whole.stats.documents_examined,
                "{}: {filter:?} read {explained:?}, unselected {whole:?}",
                self.at()
            );
        }
        // The typed read takes the query as its text reads it back
        // (SPEC §111): the same text, and the same answers.
        let from_text = self.read_back(query, filter);
        let answers = Answers {
            find: docs.find(filter.clone()).unwrap(),
            typed: typed
                .find(from_text)
                .unwrap()
                .into_iter()
                .map(|d| d.id)
                .collect(),
            cursor: docs
                .cursor(filter.clone())
                .unwrap()
                .map(Result::unwrap)
                .collect(),
            one: docs.find_one(filter.clone()).unwrap(),
            count: docs.count(filter.clone()).unwrap(),
            returned: explained.stats.documents_returned as usize,
        };
        let context = format!("{} via {:?}", self.at(), explained.plan);
        answers.agree(query, &expected, &self.model, &context);
        self.alternative_holds(filter, &explained);
        self.read_hinted(query, &expected);
        for at in 0..self.snapshots.len() {
            self.read_snapshot(at, query);
        }
    }

    /// One of the plans `explain_run` passed over, at random, run under
    /// its hint (SPEC §133): the plan it named, and the entries it
    /// counted — exactly, or more than it said.
    fn alternative_holds(&mut self, filter: &Filter, run: &trunkdb::query::Explained) {
        use trunkdb::query::{EntryCount, QueryPlan};
        if run.alternatives.is_empty() {
            return;
        }
        let other = &run.alternatives[self.program.below(run.alternatives.len())];
        let name = |fields: &[&str]| match fields {
            [one] => one.to_string(),
            several => format!("({})", several.join(", ")),
        };
        let index = |field: &str| -> Option<Vec<&'static str>> {
            if field == "_id" {
                return Some(vec!["_id"]);
            }
            INDEXES
                .iter()
                .find(|f| name(f) == field)
                .map(|f| f.to_vec())
        };
        let hinted = match &other.plan {
            QueryPlan::Scan => filter.clone().no_index(),
            QueryPlan::ById => filter.clone().use_index("_id"),
            QueryPlan::Index { field, .. } | QueryPlan::IndexOrder { field, .. } => filter
                .clone()
                .use_index(index(field).expect("a made index")),
            QueryPlan::IndexUnion { fields, .. } => match fields.first() {
                Some(first) => filter
                    .clone()
                    .use_index(index(first).expect("a made index")),
                None => return, // `false`, under any secondary index's hint
            },
            other => panic!("{}: {other:?}", self.at()),
        };
        let ran = self.docs().explain_run(hinted.clone()).unwrap();
        let context = format!("{}: {hinted} for {:?}", self.at(), other);
        assert_eq!(ran.plan, other.plan, "{context}");
        let read = ran.stats.keys_examined;
        match other.entries {
            EntryCount::Exactly(n) => assert_eq!(read, n, "{context}"),
            EntryCount::MoreThan(n) => assert!(read > n, "{context}: {read}"),
            _ => unreachable!(),
        }
        let fewer = matches!(other.entries, EntryCount::Exactly(n) if n < run.stats.keys_examined);
        assert_eq!(other.fewer, fewer, "{context}");
        self.ran.plans.insert(
            format!("alternative {:?}", other.entries)
                .split('(')
                .next()
                .unwrap()
                .to_string(),
        );
    }

    /// `filter`, of `query`, as its text reads it back (SPEC §111), which
    /// prints the same text. A closure has no text to read back
    /// (§136.5): the parser refuses one, and `filter` itself is taken.
    fn read_back(&self, query: &Query, filter: &Filter) -> Filter {
        let text = filter.to_string();
        if query.conditions.iter().any(holds_custom) {
            let refused = text.parse::<Filter>().expect_err("a closure has no text");
            assert!(refused.to_string().contains("custom(…)"), "{}", self.at());
            return filter.clone();
        }
        let from_text: Filter = text
            .parse()
            .unwrap_or_else(|e| panic!("{}: {text}: {e}", self.at()));
        assert_eq!(from_text.to_string(), text, "{}", self.at());
        from_text
    }

    /// The same read under a hint (SPEC §132): no index, the primary
    /// one, or any of `INDEXES`, made or not. A hint chooses how, never
    /// which: the same answers — or, where it can't be followed,
    /// `Error::Hint` from every read alike.
    fn read_hinted(&mut self, query: &Query, expected: &[Document]) {
        let choice = self.program.below(INDEXES.len() + 2);
        let hint = |filter: Filter| match choice {
            0 => filter.no_index(),
            1 => filter.use_index("_id"),
            at => filter.use_index(INDEXES[at - 2]),
        };
        let hinted = hint(query.filter());
        let text = hinted.to_string();
        let from_text = self.read_back(query, &hinted);
        let docs = self.docs();
        let typed = self.db().collection::<IdOnly>(COLLECTION);
        let explained = match docs.explain_run(hinted.clone()) {
            Ok(explained) => explained,
            Err(Error::Hint { .. }) => {
                let refused = |result: Result<(), Error>| matches!(result, Err(Error::Hint { .. }));
                assert!(
                    refused(docs.explain(&hinted).map(drop))
                        && refused(docs.find(hinted.clone()).map(drop))
                        && refused(typed.find(from_text).map(drop))
                        && refused(docs.cursor(hinted.clone()).map(drop))
                        && refused(docs.count(hinted.clone()).map(drop)),
                    "{}: {text} refused by explain_run only",
                    self.at()
                );
                // `find_one` reads its filter with a limit of one, which
                // a walk in sort order can follow where no limit can't.
                let first = hint(query.first());
                assert_eq!(
                    refused(docs.find_one(hinted.clone()).map(drop)),
                    refused(docs.explain(&first).map(drop)),
                    "{}: find_one of {text}",
                    self.at()
                );
                self.ran.plans.insert("refused hint".to_string());
                return;
            }
            Err(e) => panic!("{}: {text}: {e}", self.at()),
        };
        assert!(explained.hinted, "{}: {text}", self.at());
        self.ran
            .plans
            .insert(format!("hinted {}", plan_name(&explained.plan)));
        let answers = Answers {
            find: docs.find(hinted.clone()).unwrap(),
            typed: typed
                .find(from_text)
                .unwrap()
                .into_iter()
                .map(|d| d.id)
                .collect(),
            cursor: docs
                .cursor(hinted.clone())
                .unwrap()
                .map(Result::unwrap)
                .collect(),
            one: docs.find_one(hinted.clone()).unwrap(),
            count: docs.count(hinted).unwrap(),
            returned: explained.stats.documents_returned as usize,
        };
        let context = format!("{}, {text}, via {:?}", self.at(), explained.plan);
        answers.agree(query, expected, &self.model, &context);
    }

    /// Rule 2 on a kept snapshot, against the model as it was.
    fn read_snapshot(&self, at: usize, query: &Query) {
        let filter = &query.filter();
        let (snapshot, model) = &self.snapshots[at];
        let docs = snapshot.collection::<Document>(COLLECTION);
        let typed = snapshot.collection::<IdOnly>(COLLECTION);
        let expected = self.expected(model, filter);
        let explained = docs.explain_run(filter.clone()).unwrap();
        let answers = Answers {
            find: docs.find(filter.clone()).unwrap(),
            typed: typed
                .find(filter.clone())
                .unwrap()
                .into_iter()
                .map(|d| d.id)
                .collect(),
            cursor: docs
                .cursor(filter.clone())
                .unwrap()
                .map(Result::unwrap)
                .collect(),
            one: docs.find_one(filter.clone()).unwrap(),
            count: docs.count(filter.clone()).unwrap(),
            returned: explained.stats.documents_returned as usize,
        };
        let context = format!("{}, snapshot {at}, via {:?}", self.at(), explained.plan);
        answers.agree(query, &expected, model, &context);
    }
}

/// What every way of reading answered for one filter.
struct Answers {
    find: Vec<Document>,
    typed: Vec<DocId>,
    cursor: Vec<Document>,
    one: Option<Document>,
    count: usize,
    returned: usize,
}

impl Answers {
    fn agree(
        &self,
        query: &Query,
        expected: &[Document],
        model: &BTreeMap<DocId, Document>,
        context: &str,
    ) {
        let filter = query.filter();
        let n = expected.len();
        assert_eq!(self.count, n, "{context}: count of {filter:?}");
        assert_eq!(self.returned, n, "{context}: explain_run of {filter:?}");
        let ids = |docs: &[Document]| docs.iter().map(id_of).collect::<Vec<_>>();
        let first = query.first().apply(model.values().cloned());
        if !query.sort.is_empty() {
            // Ties come in id order on both sides (SPEC §105.1).
            for (how, docs) in [("find", &self.find), ("cursor", &self.cursor)] {
                let agree = docs.len() == n && docs.iter().zip(expected).all(|(a, b)| same(a, b));
                if !agree {
                    let text = |docs: &[Document]| docs.iter().map(canonical).collect::<Vec<_>>();
                    assert_eq!(text(docs), text(expected), "{context}: {how} {filter:?}");
                }
            }
            assert_eq!(
                self.typed,
                ids(&self.find),
                "{context}: typed find {filter:?}"
            );
            let one = self.one.as_ref().map(canonical);
            assert_eq!(
                one,
                first.first().map(canonical),
                "{context}: find_one {filter:?}"
            );
            return;
        }
        // No order: the same set, or, picked by a skip or limit, as many
        // distinct matches.
        let whole = query.skip == 0 && query.limit.is_none();
        let allowed: BTreeMap<DocId, Document> = (query.matching().apply(model.values().cloned()))
            .into_iter()
            .map(|doc| (id_of(&doc), doc))
            .collect();
        let allows = |doc: &Document| allowed.get(&id_of(doc)).is_some_and(|d| same(d, doc));
        for (how, docs) in [("find", &self.find), ("cursor", &self.cursor)] {
            let found: BTreeSet<DocId> = docs.iter().map(id_of).collect();
            assert_eq!(found.len(), docs.len(), "{context}: {how} twice {filter:?}");
            assert_eq!(docs.len(), n, "{context}: {how} of {filter:?}");
            if let Some(odd) = docs.iter().find(|doc| !allows(doc)) {
                panic!(
                    "{context}: {how} a non-match {filter:?}: {}",
                    canonical(odd)
                );
            }
            if whole {
                assert_eq!(found.len(), allowed.len(), "{context}: {how} {filter:?}");
            }
        }
        let mut typed = self.typed.clone();
        typed.sort();
        let typed_len = typed.len();
        typed.dedup();
        assert_eq!(
            (typed.len(), typed_len),
            (n, n),
            "{context}: typed find {filter:?}"
        );
        match &self.one {
            Some(one) => assert!(allows(one), "{context}: find_one {filter:?}"),
            None => assert!(first.is_empty(), "{context}: find_one none {filter:?}"),
        }
    }
}

/// Whether two documents are the same: fields in any order, as a
/// field's place in an object means nothing to a query; floats by their
/// bits, as a NaN isn't equal to itself.
fn same(a: &Document, b: &Document) -> bool {
    match (a, b) {
        (Document::Object(x), Document::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        (Document::Array(x), Document::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(v, w)| same(v, w))
        }
        (Document::Float(x), Document::Float(y)) => x.to_bits() == y.to_bits(),
        (x, y) => x == y,
    }
}

/// A document as text with every object's fields in name order, for a
/// failure's message.
fn canonical(doc: &Document) -> String {
    fn sorted(doc: &Document) -> Document {
        match doc {
            Document::Object(map) => {
                let mut fields: Vec<(&String, &Document)> = map.iter().collect();
                fields.sort_by(|a, b| a.0.cmp(b.0));
                Document::Object(
                    fields
                        .into_iter()
                        .map(|(k, v)| (k.clone(), sorted(v)))
                        .collect(),
                )
            }
            Document::Array(items) => Document::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    format!("{:?}", sorted(doc))
}

fn id_of(doc: &Document) -> DocId {
    match doc {
        Document::Object(map) => match map.get("_id") {
            Some(Document::Id(id)) => *id,
            other => panic!("a document without an id: {other:?}"),
        },
        other => panic!("a document that isn't an object: {other:?}"),
    }
}

fn object<const N: usize>(fields: [(&str, Document); N]) -> Document {
    Document::Object(
        fields
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

/// The values at `path` in `doc`, for conditions that match: a plain
/// path's value, each element at `tags[*]`, each element's `k`.
fn values_at(doc: &Document, path: &str) -> Vec<Document> {
    let field = |doc: &Document, name: &str| match doc {
        Document::Object(map) => map.get(name).cloned(),
        _ => None,
    };
    match path {
        "o.p" => field(doc, "o")
            .and_then(|o| field(&o, "p"))
            .into_iter()
            .collect(),
        "tags[*]" | "tags[*].k" => match field(doc, "tags") {
            Some(Document::Array(items)) => items
                .iter()
                .filter_map(|item| match path {
                    "tags[*]" => Some(item.clone()),
                    _ => field(item, "k"),
                })
                .collect(),
            _ => Vec::new(),
        },
        name => field(doc, name).into_iter().collect(),
    }
}

/// `explain`'s plan, by kind and index.
fn plan_name(plan: &trunkdb::query::QueryPlan) -> String {
    format!("{plan:?}")
}
