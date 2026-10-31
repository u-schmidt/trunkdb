# trunkdb — guide

How to use trunkdb from Rust, by example. The [README](README.md) has
the short version; [DESIGN.md](DESIGN.md) says how each part works, and
the API documentation (`cargo doc --open`) lists every method.

## Getting started

trunkdb isn't on crates.io yet. Add it from git:

```toml
[dependencies]
trunkdb = { git = "https://github.com/u-schmidt/trunkdb" }
serde = { version = "1", features = ["derive"] }
```

```rust
use serde::{Deserialize, Serialize};
use trunkdb::{Database, DocId, IndexOptions, query::{Filter, Update}};

#[derive(Serialize, Deserialize)]
struct User {
    #[serde(rename = "_id")]
    id: Option<DocId>, // None on insert: one is made; filled in on every read
    name: String,
    email: String,
    team: String,
    age: i64,
    nick: Option<String>,
    tags: Vec<String>,
}

let db = Database::open("app.trunkdb")?;
let users = db.collection::<User>("users");
users.ensure_index_with("email", IndexOptions::new().unique())?; // once at startup; a no-op after
users.ensure_index("age")?;
users.ensure_index(["team", "age"])?; // compound: by team, in age order
users.ensure_index_with("nick", IndexOptions::new().sparse())?; // no entries for users without a nick

let id = users.insert(User { id: None, name: "Ada".into(), email: "ada@example.com".into(), team: "core".into(), age: 36, nick: None, tags: vec![] })?;
let ada = users.get(&id)?.unwrap(); // ada.id == Some(id)
let same = users.find_one(Filter::new().id(id))?; // a lookup, like get
let parsed: DocId = id.to_string().parse()?; // ids go to text and back

let oldest_adults = users.find(Filter::new().gte("age", 18).sort_desc("age").limit(10))?;
let youngest_in_core = users.find(Filter::new().eq("team", "core").sort_asc("age").limit(5))?;
let by_team_then_age = users.find(Filter::new().sort_asc("team").then_asc("age").limit(20))?;
let third_page = users.find(Filter::new().sort_asc("name").skip(40).limit(20))?;

let renamed = users.update_fields(Filter::new().eq("team", "core"), &Update::new().set("team", "platform").inc("age", 1))?;
let tagged = users.update_fields(Filter::new().id(id), &Update::new().add_to_set("tags", "admin").pull("tags", "guest"))?;
```

## The id without serde's rename

With the `derive` feature (`trunkdb = { version = "0.22", features =
["derive"] }`), a struct says which field is its id, and the rename is
written for it:

```rust
#[trunkdb::document]
#[derive(Serialize, Deserialize)]
struct Task {
    #[id]
    id: Option<DocId>,
    title: String,
}
```

`#[trunkdb::document]` goes above the derive, which then sees
`#[serde(rename = "_id")]` where `#[id]` was. The id must be a `DocId`
or an `Option<DocId>`, on one field at most, or it doesn't compile.
Filters still name the field `_id`, or use `Filter::id`.

## Filters as text

A filter can also be written as text, in a language like SQL's `WHERE`,
for a search box, a config file or the command line. It compiles to the
same `Filter` the builders make (SPEC §111):

```rust
let filter: Filter = r#"team = "core" and age >= 30 order by name limit 10"#.parse()?;
let found = users.find(filter)?;

let page = Filter::new().eq("team", "core").sort_asc("name").skip(40).limit(20);
assert_eq!(page.to_string(), r#"team = "core" order by name limit 20 offset 40"#);
```

`is null`, `in (…)` for one of a list of values, `exists(…)`,
`size(…)`, `has (…)` for one element of an array, `not`/`and`/`or` and
`select` are there too; a mistake comes
back as `ParseFilterError`, with its line and column.

## A condition of your own

For a test no operator covers, a closure: name the fields it reads, and
it gets a document of just those (SPEC §136):

```rust
use trunkdb::{Document, query::Condition};

let at_example = Condition::custom(["email"], |user| {
    let email = user.get("email").and_then(Document::as_str);
    email.is_some_and(|email| email.ends_with("@example.com"))
});
let found = users.find(Filter::new().eq("team", "core").and(at_example))?;
```

The `(team, age)` index narrows the read; the closure runs on what it
leaves. A closure alone scans the collection, and has no text form. If
it panics, `find` returns `Error::ConditionPanicked`, and the database
carries on.

## Atomic batches

A batch spans collections and commits all of its writes or none:

```rust
#[derive(Serialize, Deserialize)]
struct Event {
    user: DocId,
    what: String,
}

let events = db.collection::<Event>("events");
let mut batch = db.batch();
let grace = batch.insert(&users, User { id: None, name: "Grace".into(), email: "grace@example.com".into(), team: "core".into(), age: 45, nick: None, tags: vec![] })?;
batch.insert(&events, Event { user: grace, what: "joined".into() })?; // the id is known before the commit
batch.commit()?; // both, or neither: after a crash too
```

## Hearing about changes

`subscribe` hands out a receiver that hears of every batch committed
after it, from any thread (SPEC §135):

```rust
use trunkdb::Change;

let changes = db.subscribe()?;
let watcher = std::thread::spawn(move || {
    for commit in changes {                // waits for each commit
        for change in &commit.changes {
            match change {
                Change::Inserted { collection, id, .. } => println!("new in {collection}: {id}"),
                Change::Updated { collection, id, .. } => println!("{id} in {collection} written"),
                Change::Deleted { collection, id, .. } => println!("{id} gone from {collection}"),
                Change::Dropped { collection, .. } => println!("{collection} dropped"),
                _ => {}
            }
        }
    }                                      // ends when the database is dropped
});
```

- **One report per batch**, in the order they committed, sent once the
  batch is durable and reads see it: a `get` of the id finds it. A
  batch that failed is never reported.
- **Each document once, with what the batch did to it in all:**
  inserted and then updated is `Inserted`, inserted and deleted in one
  batch isn't listed at all. `drop_collection` is one `Dropped`.
- **Ids, not documents:** read the document if you need it. A later
  commit may have changed it since.
- **The writer never waits** for a subscriber, so the receiving thread
  may write to the database itself. Reports it hasn't read yet wait in
  memory.
- **A report is lost if the program stops** between a commit and its
  report. Where an event must not be lost, write it as a document in
  the same batch as the change, and work those off.

## Reads that agree with each other

Each call reads the database as it is at that moment, so a write on
another thread can land between two calls. A snapshot is one moment
for as many reads as you like, in every collection, and holds up
nobody:

```rust
let snapshot = db.snapshot()?;
let users = snapshot.collection::<User>("users"); // a View<User>: reads only
let how_many = users.count(Filter::new())?;
let everyone = users.find(Filter::new())?; // how_many == everyone.len(), always
snapshot.export(std::fs::File::create("backup.jsonl")?)?; // the same moment
```

Drop it when the reads are done: while it's open, pages changed since
stay in memory as they were, and `compact` is refused. A `cursor` is a
snapshot of its own, for as long as it's iterated.

That memory has a limit, 256 MiB unless `OpenOptions::snapshot_memory`
says otherwise. A snapshot left open beside so many writes that it's
passed is ended, the oldest first: its reads return
`Error::SnapshotTooOld`, and a new snapshot reads on. Writes are never
held up or refused for it. `db.snapshot_info()` shows how much is kept.

## Reading only some fields

`select` hands out only the fields it names, and each document's `_id`,
as a SQL column list does (SPEC §106):

```rust
use trunkdb::Document;

let rows = db
    .collection::<Document>("users")
    .find(Filter::new().eq("team", "core").select(["name", "address.city"]))?;
// [{"_id": ..., "name": "Ada", "address": {"city": "London"}}, ...]
```

- `_id` comes first, then the paths in the order selected, nested as
  stored.
- A missing field is left out; a stored `null` is kept.
- A path doesn't walk into arrays: `items[*].sku` selects `items` whole.
- The filter and the sort see the whole document.
- Writes ignore it: `update_many` is given the whole document, since
  what it returns is written back.

The fields not selected are skipped, not decoded. Where an index holds
the filter's fields and every selected one, a match comes from its index
entry and the document isn't read at all: `(status, created)` answers
"the queued, newest first, their status and creation" from the index
alone, if `created` is a date-time. Booleans, strings, ids and
date-times can come from an entry. Numbers and nulls are read from the
document, because the key keeps neither a number's type nor whether a
null field is there.

A typed collection can also be opened with a smaller type: serde
ignores the fields it doesn't declare, so this hands back only names
and teams:

```rust
#[derive(Serialize, Deserialize)]
struct UserSummary {
    name: String,
    team: String,
}

let summaries = db.collection::<UserSummary>("users").find(Filter::new().eq("team", "core"))?;
```

Read through it, never write: `update`, `upsert` and `insert` store the
whole value, so a `UserSummary` written back replaces the user and
drops every field it lacks. To change some fields, use `update_fields`.
A typed read decodes each document straight into the type, so the
fields a summary doesn't declare are skipped, not built (SPEC §99).

## Counting and summing by group

`aggregate` puts the documents a filter finds in groups and sums them up,
as SQL's `GROUP BY` does (SPEC §107):

```rust
use trunkdb::query::{Aggregate, Condition};

let per_status = db.collection::<Document>("tasks").aggregate(
    Filter::new().eq("tenant", "t1"),
    &Aggregate::new()
        .group_by("status")
        .count().named("n")                  // the group's documents
        .avg("tries").named("average_tries") // the mean of `tries`
        .max("created").named("newest")      // the largest `created`
        .having(Condition::gt("n", 10))      // rows whose "n" is over 10
        .sort_desc("n"),
)?;
// [{"status": "Done", "n": 340, "average_tries": 4.5, "newest": ...}, ...]
```

- A column says what it computes, then `named` gives it its name in
  the row, as SQL's `AS` does: `sum("tries").named("total")` is
  `sum(tries) AS total`. A column without a name doesn't compile.
- The filter picks the documents, sort and limit included, as for
  `find`; the rows have the group's values and each column, no `_id`.
- A missing field groups with `null`; `5` and `5.0` are one group.
- `sum` and `avg` add only numbers, exactly, in any order; a mean is
  rounded once, so it is finite wherever the numbers are; `min` and
  `max` pass over nulls.
- Without `group_by`, one row of everything, even of nothing.

A column can count or sum only some of a group's documents: `filter`
before its `named` gives that one column a condition, SQL's `FILTER
(WHERE …)` (SPEC §126). The groups and the other columns still see
every document the filter finds:

```rust
let per_team = db.collection::<Document>("tasks").aggregate(
    Filter::new(),
    &Aggregate::new()
        .group_by("team")
        .count().named("tasks") // every task of the team
        .count()                // only the open ones
        .filter(Condition::eq("status", "open"))
        .named("open_tasks")
        .avg("hours")           // the mean of `hours`, only over hours >= 4
        .filter(Condition::gte("hours", 4))
        .named("mean_long_hours"),
)?;
// [{"team": "a", "tasks": 12, "open_tasks": 3, "mean_long_hours": 6.5},
//  {"team": "b", "tasks": 4, "open_tasks": 0, "mean_long_hours": null}]
```

- `filter` belongs to its column, not to the aggregate:
  to leave documents out of every column, put the condition in the
  `Filter` instead. Then a team with no open task has no row at all.
- A team none of whose documents meets a column's condition is still a
  row: `0` for a `count` or `sum`, `null` for the others.
- Two `filter`s on one column are ANDed.

Where an index holds the fields an aggregate reads, its entries answer
`count`, `avg`, `min` and `max` without reading the documents: "tasks
per status" on `(status, created)` counts the index's leaves. `sum`
reads its numbers from the documents, as the key doesn't keep whether
a number is an `Int` or a `Float`. A column's condition is decided from
the entries too, where the index holds the fields it names.
`explain_aggregate` says how it ran.

## Telling the planner which index

The planner follows fixed rules, which don't look at the data. Where
they pick badly, say which index in the query:

```rust
// Nearly every task is "done": the rules read the whole `status = "done"`
// range, where the few created since yesterday are a short range.
let recent = Filter::new()
    .eq("status", "done")
    .gt("created", yesterday)
    .use_index("created");
// As text: status = "done" and created > … use index created
```

- `use_index` takes the fields as `ensure_index` does: `"created"`,
  `["status", "created"]`. `"_id"` is the primary index.
- `no_index()` scans every document, lookups by id too.
- The results are the same with any hint. Only how they're found
  changes.
- A hint that can't be followed is `Error::Hint`, from every read:
  there's no index on those fields, or the filter has no condition on
  it (nor a sort with a limit it can give). Dropping an index a hint
  names makes those reads fail, so you hear about it.
- `explain_run` says `hinted`, and `trunkdb find … --explain` says
  `as hinted`.

`explain_run` also lists the plans it passed over, in `alternatives`,
each with the index entries it would read. That tells you whether a
hint pays before you add it, and whether it still does afterwards:

```
plan: index on status
read: status, where status = "done", 190 entries
instead: index on created, 10 entries, fewer
instead: scan of every document, 200 entries
```

An index range is counted whole, without reading documents, so the
numbers show how much a hint saves. Past about 100,000 entries, it
says "more than". A walk in sort order is followed only as far as the
query itself read.
