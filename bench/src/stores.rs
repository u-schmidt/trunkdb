//! One adapter per database. trunkdb is a document store; the others
//! get what a user would build on them for the same job: SQLite a table
//! with the indexed fields as columns and the document as JSON, redb and
//! sled a table of JSON documents by id plus one table of keys per
//! index, kept up to date in the same transaction.

use crate::{Id, Task};
use std::path::{Path, PathBuf};

pub trait Store {
    fn name(&self) -> &'static str;
    /// One durable, atomic commit.
    fn insert(&mut self, tasks: &[(Id, Task)]);
    /// One durable, atomic commit; each id exists.
    fn update(&mut self, tasks: &[(Id, Task)]);
    /// One durable, atomic commit; each id exists.
    fn delete(&mut self, ids: &[Id]);
    fn get(&self, id: &Id) -> Option<Task>;
    /// How many tasks the tenant has — each read and decoded.
    fn by_tenant(&self, tenant: &str) -> usize;
    /// How many tasks were created in `lo..hi` — each read and decoded.
    fn in_range(&self, lo: i64, hi: i64) -> usize;
    /// `created` of the oldest `limit` tasks with `status`.
    fn oldest(&self, status: &str, limit: usize) -> Vec<i64>;
    /// `created` of the newest `limit` tasks with `status`.
    fn newest(&self, status: &str, limit: usize) -> Vec<i64>;
    /// How many tasks have `tries > n`: no index, every document.
    fn count_tries_above(&self, n: i64) -> usize;
    /// Bytes on disk.
    fn size(&self) -> u64;
    /// Rebuilds the file as small as it gets; `false` if the store
    /// can't.
    fn compact(&mut self) -> bool;
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

fn dir_size(path: &Path) -> u64 {
    std::fs::read_dir(path).map_or(0, |entries| {
        entries
            .flatten()
            .map(|entry| match entry.file_type() {
                Ok(kind) if kind.is_dir() => dir_size(&entry.path()),
                _ => file_size(&entry.path()),
            })
            .sum()
    })
}

// ---- trunkdb ---------------------------------------------------------

use trunkdb::query::Filter;
use trunkdb::{Collection, Database, DocId, Document};

pub struct Trunk {
    db: Database,
    tasks: Collection<Task>,
    /// The same collection, untyped: batches write each task under the
    /// id every store uses, which a `Task` has no field for.
    docs: Collection<Document>,
    path: PathBuf,
}

impl Trunk {
    pub fn open(dir: &Path) -> Self {
        let path = dir.join("bench.trunkdb");
        // The default cache unless `BENCH_TRUNKDB_CACHE_MB` says (redb's
        // and sled's default is 1 GiB, SQLite's 2 MB).
        let mut options = trunkdb::OpenOptions::default();
        if let Ok(mb) = std::env::var("BENCH_TRUNKDB_CACHE_MB") {
            options = options.cache_size(mb.parse::<usize>().unwrap() << 20);
        }
        // And the default checkpoint threshold unless
        // `BENCH_TRUNKDB_CHECKPOINT_PAGES` says.
        if let Ok(pages) = std::env::var("BENCH_TRUNKDB_CHECKPOINT_PAGES") {
            options = options.checkpoint_pages(pages.parse().unwrap());
        }
        let db = Database::open_with(&path, options).unwrap();
        let tasks = db.collection::<Task>("tasks");
        tasks.ensure_index("tenant").unwrap();
        tasks.ensure_index("created").unwrap();
        tasks.ensure_index(["status", "created"]).unwrap();
        let docs = db.collection::<Document>("tasks");
        Trunk {
            db,
            tasks,
            docs,
            path,
        }
    }
}

/// `task` as a document holding `id` as its `_id`, which an insert uses.
fn document(id: &Id, task: &Task) -> Document {
    let mut doc = Document::from_value(task).unwrap();
    if let Document::Object(fields) = &mut doc {
        fields.insert("_id".into(), Document::Id(DocId(*id)));
    }
    doc
}

impl Store for Trunk {
    fn name(&self) -> &'static str {
        "trunkdb"
    }

    fn insert(&mut self, tasks: &[(Id, Task)]) {
        let mut batch = self.db.batch();
        for (id, task) in tasks {
            batch.insert(&self.docs, document(id, task)).unwrap();
        }
        batch.commit().unwrap();
    }

    fn update(&mut self, tasks: &[(Id, Task)]) {
        let mut batch = self.db.batch();
        for (id, task) in tasks {
            batch
                .update(&self.docs, &DocId(*id), document(id, task))
                .unwrap();
        }
        batch.commit().unwrap();
    }

    fn delete(&mut self, ids: &[Id]) {
        let mut batch = self.db.batch();
        for id in ids {
            batch.delete(&self.docs, &DocId(*id));
        }
        batch.commit().unwrap();
    }

    fn get(&self, id: &Id) -> Option<Task> {
        self.tasks.get(&DocId(*id)).unwrap()
    }

    fn by_tenant(&self, tenant: &str) -> usize {
        self.tasks
            .find(Filter::new().eq("tenant", tenant))
            .unwrap()
            .len()
    }

    fn in_range(&self, lo: i64, hi: i64) -> usize {
        let range = Filter::new().gte("created", lo).lt("created", hi);
        self.tasks.find(range).unwrap().len()
    }

    fn oldest(&self, status: &str, limit: usize) -> Vec<i64> {
        let f = Filter::new()
            .eq("status", status)
            .sort_asc("created")
            .limit(limit);
        self.tasks
            .find(f)
            .unwrap()
            .iter()
            .map(|t| t.created)
            .collect()
    }

    fn newest(&self, status: &str, limit: usize) -> Vec<i64> {
        let f = Filter::new()
            .eq("status", status)
            .sort_desc("created")
            .limit(limit);
        self.tasks
            .find(f)
            .unwrap()
            .iter()
            .map(|t| t.created)
            .collect()
    }

    fn count_tries_above(&self, n: i64) -> usize {
        self.tasks.count(Filter::new().gt("tries", n)).unwrap()
    }

    fn size(&self) -> u64 {
        file_size(&self.path) + file_size(&self.path.with_extension("trunkdb.wal"))
    }

    fn compact(&mut self) -> bool {
        self.db.compact().unwrap();
        true
    }
}

// ---- SQLite ----------------------------------------------------------

use rusqlite::{Connection, params};

pub struct Sqlite {
    conn: Connection,
    path: PathBuf,
}

impl Sqlite {
    pub fn open(dir: &Path) -> Self {
        let path = dir.join("bench.sqlite");
        let conn = Connection::open(&path).unwrap();
        // Durable at every commit, as the others are.
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "synchronous", "FULL").unwrap();
        // On macOS a plain `fsync` doesn't reach the disk; Rust's
        // `sync_all`/`sync_data`, which the others use, issue
        // `F_FULLFSYNC`. So does SQLite with these.
        conn.pragma_update(None, "fullfsync", "ON").unwrap();
        conn.pragma_update(None, "checkpoint_fullfsync", "ON")
            .unwrap();
        conn.execute_batch(
            "CREATE TABLE tasks (
                id BLOB PRIMARY KEY,
                status TEXT NOT NULL,
                created INTEGER NOT NULL,
                tenant TEXT NOT NULL,
                doc TEXT NOT NULL
            );
            CREATE INDEX tasks_tenant ON tasks (tenant);
            CREATE INDEX tasks_created ON tasks (created);
            CREATE INDEX tasks_status_created ON tasks (status, created);",
        )
        .unwrap();
        Sqlite { conn, path }
    }

    fn docs(&self, sql: &str, params: impl rusqlite::Params) -> Vec<Task> {
        let mut statement = self.conn.prepare_cached(sql).unwrap();
        statement
            .query_map(params, |row| row.get::<_, String>(0))
            .unwrap()
            .map(|doc| serde_json::from_str(&doc.unwrap()).unwrap())
            .collect()
    }
}

impl Store for Sqlite {
    fn name(&self) -> &'static str {
        "SQLite"
    }

    fn insert(&mut self, tasks: &[(Id, Task)]) {
        let tx = self.conn.transaction().unwrap();
        {
            let mut insert = tx
                .prepare_cached(
                    "INSERT INTO tasks (id, status, created, tenant, doc) VALUES (?, ?, ?, ?, ?)",
                )
                .unwrap();
            for (id, t) in tasks {
                let doc = serde_json::to_string(t).unwrap();
                insert
                    .execute(params![&id[..], t.status, t.created, t.tenant, doc])
                    .unwrap();
            }
        }
        tx.commit().unwrap();
    }

    fn update(&mut self, tasks: &[(Id, Task)]) {
        let tx = self.conn.transaction().unwrap();
        {
            let mut update = tx
                .prepare_cached(
                    "UPDATE tasks SET status = ?, created = ?, tenant = ?, doc = ? WHERE id = ?",
                )
                .unwrap();
            for (id, t) in tasks {
                let doc = serde_json::to_string(t).unwrap();
                let changed = update
                    .execute(params![t.status, t.created, t.tenant, doc, &id[..]])
                    .unwrap();
                assert_eq!(changed, 1);
            }
        }
        tx.commit().unwrap();
    }

    fn delete(&mut self, ids: &[Id]) {
        let tx = self.conn.transaction().unwrap();
        {
            let mut delete = tx.prepare_cached("DELETE FROM tasks WHERE id = ?").unwrap();
            for id in ids {
                assert_eq!(delete.execute([&id[..]]).unwrap(), 1);
            }
        }
        tx.commit().unwrap();
    }

    fn get(&self, id: &Id) -> Option<Task> {
        self.docs("SELECT doc FROM tasks WHERE id = ?", [&id[..]])
            .pop()
    }

    fn by_tenant(&self, tenant: &str) -> usize {
        self.docs("SELECT doc FROM tasks WHERE tenant = ?", [tenant])
            .len()
    }

    fn in_range(&self, lo: i64, hi: i64) -> usize {
        self.docs(
            "SELECT doc FROM tasks WHERE created >= ? AND created < ?",
            [lo, hi],
        )
        .len()
    }

    fn oldest(&self, status: &str, limit: usize) -> Vec<i64> {
        self.docs(
            "SELECT doc FROM tasks WHERE status = ? ORDER BY created LIMIT ?",
            params![status, limit as i64],
        )
        .iter()
        .map(|t| t.created)
        .collect()
    }

    fn newest(&self, status: &str, limit: usize) -> Vec<i64> {
        self.docs(
            "SELECT doc FROM tasks WHERE status = ? ORDER BY created DESC LIMIT ?",
            params![status, limit as i64],
        )
        .iter()
        .map(|t| t.created)
        .collect()
    }

    fn count_tries_above(&self, n: i64) -> usize {
        // What one would write in SQLite: its own JSON functions.
        self.conn
            .query_row(
                "SELECT count(*) FROM tasks WHERE json_extract(doc, '$.tries') > ?",
                [n],
                |row| row.get::<_, i64>(0),
            )
            .unwrap() as usize
    }

    fn size(&self) -> u64 {
        let wal = PathBuf::from(format!("{}-wal", self.path.display()));
        file_size(&self.path) + file_size(&wal)
    }

    fn compact(&mut self) -> bool {
        self.conn
            .execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        true
    }
}

// ---- keys for the key-value stores' indexes ----------------------------

/// An `i64` as bytes that sort as the numbers do.
fn sortable(n: i64) -> [u8; 8] {
    ((n as u64) ^ (1 << 63)).to_be_bytes()
}

fn tenant_prefix(tenant: &str) -> Vec<u8> {
    [tenant.as_bytes(), &[0]].concat()
}

fn status_prefix(status: &str) -> Vec<u8> {
    [status.as_bytes(), &[0]].concat()
}

/// A task's key in each index: the indexed values, then the id.
fn index_keys(id: &Id, t: &Task) -> [Vec<u8>; 3] {
    [
        [tenant_prefix(&t.tenant).as_slice(), id].concat(),
        [&sortable(t.created)[..], id].concat(),
        [
            status_prefix(&t.status).as_slice(),
            &sortable(t.created),
            id,
        ]
        .concat(),
    ]
}

/// Every key starting with `prefix`: `prefix` up to `prefix` with its
/// last byte raised (it ends in a 0, so there is one to raise).
fn prefix_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    *end.last_mut().unwrap() += 1;
    end
}

fn id_of(key: &[u8]) -> Id {
    key[key.len() - 16..].try_into().unwrap()
}

// ---- redb ------------------------------------------------------------

use redb::{ReadableDatabase, ReadableTable, TableDefinition};

const TASKS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("tasks");
const INDEXES: [TableDefinition<&[u8], ()>; 3] = [
    TableDefinition::new("by_tenant"),
    TableDefinition::new("by_created"),
    TableDefinition::new("by_status_created"),
];

pub struct Redb {
    db: redb::Database,
    path: PathBuf,
}

impl Redb {
    pub fn open(dir: &Path) -> Self {
        let path = dir.join("bench.redb");
        let db = redb::Database::create(&path).unwrap();
        // Create the tables, so reads find them.
        let tx = db.begin_write().unwrap();
        tx.open_table(TASKS).unwrap();
        for index in INDEXES {
            tx.open_table(index).unwrap();
        }
        tx.commit().unwrap();
        Redb { db, path }
    }

    /// Writes `changes` — a new task, or none to delete — in one commit,
    /// with the old task's index keys removed and the new one's added.
    fn write(&self, changes: impl Iterator<Item = (Id, Option<Task>)>) {
        let tx = self.db.begin_write().unwrap();
        {
            let mut tasks = tx.open_table(TASKS).unwrap();
            let mut indexes = INDEXES.map(|index| tx.open_table(index).unwrap());
            for (id, new) in changes {
                let old: Option<Task> = tasks
                    .get(&id[..])
                    .unwrap()
                    .map(|doc| serde_json::from_slice(doc.value()).unwrap());
                if let Some(old) = &old {
                    for (index, key) in indexes.iter_mut().zip(index_keys(&id, old)) {
                        index.remove(key.as_slice()).unwrap();
                    }
                }
                match new {
                    Some(new) => {
                        let doc = serde_json::to_vec(&new).unwrap();
                        tasks.insert(&id[..], doc.as_slice()).unwrap();
                        for (index, key) in indexes.iter_mut().zip(index_keys(&id, &new)) {
                            index.insert(key.as_slice(), ()).unwrap();
                        }
                    }
                    None => {
                        tasks.remove(&id[..]).unwrap();
                    }
                }
            }
        }
        tx.commit().unwrap();
    }

    /// The tasks whose keys in index `which` lie in `lo..hi`, in key
    /// order or the reverse, at most `limit`.
    fn through(
        &self,
        which: usize,
        lo: &[u8],
        hi: &[u8],
        limit: usize,
        backward: bool,
    ) -> Vec<Task> {
        let tx = self.db.begin_read().unwrap();
        let tasks = tx.open_table(TASKS).unwrap();
        let index = tx.open_table(INDEXES[which]).unwrap();
        let range = index.range(lo..hi).unwrap();
        let entries: Box<dyn Iterator<Item = _>> = match backward {
            true => Box::new(range.rev()),
            false => Box::new(range),
        };
        entries
            .take(limit)
            .map(|entry| {
                let id = id_of(entry.unwrap().0.value());
                let doc = tasks.get(&id[..]).unwrap().unwrap();
                serde_json::from_slice(doc.value()).unwrap()
            })
            .collect()
    }
}

impl Store for Redb {
    fn name(&self) -> &'static str {
        "redb"
    }

    fn insert(&mut self, tasks: &[(Id, Task)]) {
        self.write(tasks.iter().map(|(id, t)| (*id, Some(t.clone()))));
    }

    fn update(&mut self, tasks: &[(Id, Task)]) {
        self.write(tasks.iter().map(|(id, t)| (*id, Some(t.clone()))));
    }

    fn delete(&mut self, ids: &[Id]) {
        self.write(ids.iter().map(|id| (*id, None)));
    }

    fn get(&self, id: &Id) -> Option<Task> {
        let tx = self.db.begin_read().unwrap();
        let tasks = tx.open_table(TASKS).unwrap();
        let doc = tasks.get(&id[..]).unwrap()?;
        Some(serde_json::from_slice(doc.value()).unwrap())
    }

    fn by_tenant(&self, tenant: &str) -> usize {
        let lo = tenant_prefix(tenant);
        self.through(0, &lo, &prefix_end(&lo), usize::MAX, false)
            .len()
    }

    fn in_range(&self, lo: i64, hi: i64) -> usize {
        self.through(1, &sortable(lo), &sortable(hi), usize::MAX, false)
            .len()
    }

    fn oldest(&self, status: &str, limit: usize) -> Vec<i64> {
        let lo = status_prefix(status);
        self.through(2, &lo, &prefix_end(&lo), limit, false)
            .iter()
            .map(|t| t.created)
            .collect()
    }

    fn newest(&self, status: &str, limit: usize) -> Vec<i64> {
        let lo = status_prefix(status);
        self.through(2, &lo, &prefix_end(&lo), limit, true)
            .iter()
            .map(|t| t.created)
            .collect()
    }

    fn count_tries_above(&self, n: i64) -> usize {
        let tx = self.db.begin_read().unwrap();
        let tasks = tx.open_table(TASKS).unwrap();
        tasks
            .iter()
            .unwrap()
            .filter(|entry| {
                let task: Task = serde_json::from_slice(entry.as_ref().unwrap().1.value()).unwrap();
                task.tries > n
            })
            .count()
    }

    fn size(&self) -> u64 {
        file_size(&self.path)
    }

    fn compact(&mut self) -> bool {
        while self.db.compact().unwrap() {}
        true
    }
}

// ---- sled ------------------------------------------------------------

use sled::Transactional;
use sled::transaction::{ConflictableTransactionError, TransactionalTree};

pub struct Sled {
    db: sled::Db,
    tasks: sled::Tree,
    indexes: [sled::Tree; 3],
    dir: PathBuf,
}

impl Sled {
    pub fn open(dir: &Path) -> Self {
        let dir = dir.join("bench.sled");
        let db = sled::open(&dir).unwrap();
        let tasks = db.open_tree("tasks").unwrap();
        let indexes = ["by_tenant", "by_created", "by_status_created"]
            .map(|name| db.open_tree(name).unwrap());
        Sled {
            db,
            tasks,
            indexes,
            dir,
        }
    }

    /// As `Redb::write`, in one transaction over the four trees, then
    /// flushed: sled only promises durability after a flush.
    fn write(&self, changes: &[(Id, Option<Task>)]) {
        let [a, b, c] = &self.indexes;
        (&self.tasks, a, b, c)
            .transaction(|(tasks, a, b, c)| {
                let indexes: [&TransactionalTree; 3] = [a, b, c];
                for (id, new) in changes {
                    if let Some(old) = tasks.get(&id[..])? {
                        let old: Task = serde_json::from_slice(&old).unwrap();
                        for (index, key) in indexes.iter().zip(index_keys(id, &old)) {
                            index.remove(key)?;
                        }
                    }
                    match new {
                        Some(new) => {
                            tasks.insert(&id[..], serde_json::to_vec(new).unwrap())?;
                            for (index, key) in indexes.iter().zip(index_keys(id, new)) {
                                index.insert(key, &[][..])?;
                            }
                        }
                        None => {
                            tasks.remove(&id[..])?;
                        }
                    }
                }
                Ok::<(), ConflictableTransactionError<()>>(())
            })
            .unwrap();
        self.db.flush().unwrap();
    }

    fn through(
        &self,
        which: usize,
        lo: &[u8],
        hi: &[u8],
        limit: usize,
        backward: bool,
    ) -> Vec<Task> {
        let range = self.indexes[which].range(lo..hi);
        let entries: Box<dyn Iterator<Item = _>> = match backward {
            true => Box::new(range.rev()),
            false => Box::new(range),
        };
        entries
            .take(limit)
            .map(|entry| {
                let id = id_of(&entry.unwrap().0);
                let doc = self.tasks.get(id).unwrap().unwrap();
                serde_json::from_slice(&doc).unwrap()
            })
            .collect()
    }
}

impl Store for Sled {
    fn name(&self) -> &'static str {
        "sled"
    }

    fn insert(&mut self, tasks: &[(Id, Task)]) {
        let changes: Vec<_> = tasks.iter().map(|(id, t)| (*id, Some(t.clone()))).collect();
        self.write(&changes);
    }

    fn update(&mut self, tasks: &[(Id, Task)]) {
        self.insert(tasks);
    }

    fn delete(&mut self, ids: &[Id]) {
        let changes: Vec<_> = ids.iter().map(|id| (*id, None)).collect();
        self.write(&changes);
    }

    fn get(&self, id: &Id) -> Option<Task> {
        let doc = self.tasks.get(id).unwrap()?;
        Some(serde_json::from_slice(&doc).unwrap())
    }

    fn by_tenant(&self, tenant: &str) -> usize {
        let lo = tenant_prefix(tenant);
        self.through(0, &lo, &prefix_end(&lo), usize::MAX, false)
            .len()
    }

    fn in_range(&self, lo: i64, hi: i64) -> usize {
        self.through(1, &sortable(lo), &sortable(hi), usize::MAX, false)
            .len()
    }

    fn oldest(&self, status: &str, limit: usize) -> Vec<i64> {
        let lo = status_prefix(status);
        self.through(2, &lo, &prefix_end(&lo), limit, false)
            .iter()
            .map(|t| t.created)
            .collect()
    }

    fn newest(&self, status: &str, limit: usize) -> Vec<i64> {
        let lo = status_prefix(status);
        self.through(2, &lo, &prefix_end(&lo), limit, true)
            .iter()
            .map(|t| t.created)
            .collect()
    }

    fn count_tries_above(&self, n: i64) -> usize {
        self.tasks
            .iter()
            .filter(|entry| {
                let task: Task = serde_json::from_slice(&entry.as_ref().unwrap().1).unwrap();
                task.tries > n
            })
            .count()
    }

    fn size(&self) -> u64 {
        self.db.flush().unwrap();
        dir_size(&self.dir)
    }

    fn compact(&mut self) -> bool {
        false
    }
}
