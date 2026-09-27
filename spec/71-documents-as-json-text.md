# 71. Documents as JSON text (`document.rs`, `json.rs`)

A tool that isn't written against a Rust struct — an editor, a sync
bridge, a web front end — works in JSON. trunkdb has read and written
JSON since export and import (§30), but only there: `to_json` and
`from_json` were `pub` inside a private module, which reaches no one
outside the crate. Now a `Document` is JSON text both ways:

```rust
use trunkdb::{Database, Document};

let people = db.collection::<Document>("people");
let doc: Document = r#"{"name": "Ann", "born": {"$date": "1990-05-01T12:00:00Z"}}"#.parse()?;
let id = people.insert(doc)?;

let text = people.get(&id)?.unwrap().to_string();
// {"_id":{"$id":"0192…"},"name":"Ann","born":{"$date":"1990-05-01T12:00:00Z"}}
let pretty = format!("{:#}", people.get(&id)?.unwrap());   // indented
people.update(&id, text.replace("Ann", "Bea").parse()?)?;
```

## 71.1 The API
- **`Display` for `Document`:** compact JSON; `{:#}` indents it. So
  `doc.to_string()` and `println!("{doc}")` give JSON. `{:?}` is still
  the Rust-shaped debug form.
- **`FromStr` for `Document`:** `text.parse::<Document>()`, with
  **`ParseDocumentError`**, whose message says what's wrong: `not JSON:
  … at line 2 column 5`, an integer outside `i64`, a malformed tag.
- The same pair `DocId` (§63) and `DateTime` (§70) already have: a
  value goes to text with `to_string` and back with `parse`, equal to
  what it was.
- A tool then uses `Collection<Document>` as it is: `insert`, `get`,
  `update`, `find`, `upsert`, `update_fields`. No `insert_json`-style
  methods: they would only wrap `parse` and `to_string`.

## 71.2 The text
Tagged JSON, the one an export writes (§30.1): plain JSON, except for
what JSON has no type for — `{"$id": "<uuid>"}`, `{"$date": "<RFC
3339>"}`, `{"$binary": "<base64>"}`, `{"$float": "NaN"}`. An `Int` is
an integer and a `Float` always has a fraction or exponent (`2.0`), so
the two stay apart. Plain JSON is valid tagged JSON, so a tool that
knows nothing of tags can still write documents; it only gets strings
where it could have had ids or date-times.

It is the export's document form, not its line: a non-object document
is just its value (`5`, `[1, 2]`), with no `{"_id": …, "$value": …}`
wrapper, since the id isn't part of the text here.

Nesting: `serde_json` stops at 128 levels, so deep text is an error,
never a stack overflow; a write still refuses more than 64 (§56).

## 71.3 The id
A stored object carries its id as `"_id": {"$id": "<uuid>"}`, first
(§59), and the text keeps it as that tag, so it parses back to a
`Document::Id`. So:
- **insert** of such text keeps the id, as it keeps any `_id` holding
  an id (§59); **update** takes its id from its argument, and the `_id`
  in the text is ignored, as for any document.
- An `_id` that is a plain string, `"_id": "0192…"`, is a string, not
  an id: `parse` reads what the text says. An insert then makes a new
  id, as for any `_id` that isn't an id (§59). A tool that writes ids
  itself writes the tag.

## 71.4 Why a `String`, not `serde_json::Value`
`serde_json` appears nowhere in trunkdb's public API; `export` and
`import` take `Write` and `Read`. Returning `serde_json::Value` would
make it a public dependency: a `serde_json` 2 would be a type
incompatible with 1's, and following it a breaking change of trunkdb's
own. How others do it:
- The Rust API Guidelines (C-STABLE): a crate is stable only if its
  public dependencies are.
- `tokio-postgres` supports `serde_json::Value` behind a feature named
  `with-serde_json-1`, the major version in its name; `rusqlite` and
  `sqlx` behind optional features too.
- LiteDB, trunkdb's model, turns a `BsonValue` into a JSON string and
  back.

Text costs a tool one more parse when it wants to look inside, which
doesn't matter at a tool's speed. And adding a `Value` form later, if
someone needs it, breaks nothing; taking one away would.

Rejected:
- **`to_json() -> serde_json::Value` now:** the public dependency
  above, for a gain nobody has asked for yet. Open, behind a feature,
  if someone does.
- **`Document::to_json_string()` / `from_json_str()`:** `Display` and
  `FromStr` are how Rust says "this has a text form", and what
  `DocId` and `DateTime` already use.
- **Reading a plain string `_id` as an id when it looks like a UUID:**
  a guess; a string field may hold a UUID and mean a string. Tagged
  JSON never guesses (§30.1).

Nothing on disk or in exports changes.
