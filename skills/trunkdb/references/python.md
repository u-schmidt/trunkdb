# trunkdb from Python

The `trunkdb` module (`bindings/python/` in trunkdb's repository) opens
the same files the command does. Filters and aggregates are the same
text; parameters are a dict, `{"s": "open"}`, written `:s` in the text.
Documents come back as dicts: `_id` a `trunkdb.DocId`, dates and times
as `datetime`, `date` and `time`, binary as `bytes`.

```python
import trunkdb

with trunkdb.open("app.trunkdb", read_only=True) as db:
    users = db.collection("users")
    print(db.collections(), users.count(), users.indexes())
    for doc in users.find("age >= :min limit 10", {"min": 30}):
        print(doc)
```

Open with `read_only=True` to look: it opens beside an application that
has the file open for writing, and writes nothing. Without it, the open
fails with `BlockingIOError` while another program writes the file.

| Command | Python |
|---|---|
| `info` | `db.collections()`, `db.collection(c).count()`, `.indexes()`, `db.stats()` |
| `find '<filter>'` | `coll.find(filter, params)`; one at a time, `coll.cursor(filter, params)` |
| `count '<filter>'` | `coll.count(filter, params)` |
| `aggregate '<filter>' '<aggregate>'` | `coll.aggregate(filter, aggregate, params)` |
| `find '<filter>' --explain` | `coll.explain(filter, params)` |
| `get <id>` | `coll.get(id)` |
| `insert`, `update`, `delete` | `coll.insert(doc)`, `coll.update_many(filter, update, params)`, `coll.delete_many(filter, params)` |
| `batch`, and any write to try first | `db.batch(ops, params, dry_run=True)`, then without `dry_run` |
| `index <collection> add <field>...` | `coll.ensure_index(fields, unique=False, sparse=False)` |
| `check` | `db.check()` |
| `export <file>` | `db.export(path)` |
| `watch '<filter>'` | `for event in coll.watch(filter, params):` |

`batch`'s ops are dicts shaped as the lines `trunkdb batch` reads,
`{"op": "update", "collection": "users", "filter": "age < 30",
"update": {"$set": {"young": True}}}`, or those lines as one text.

`fields` has no call of its own: use the command, or look at a few
documents with a `limit`.

Errors are exceptions: `trunkdb.ParseError` for a text that doesn't
read (its `offset` says where), `trunkdb.DuplicateValueError` for a
unique index, `trunkdb.Error` for all of them.
