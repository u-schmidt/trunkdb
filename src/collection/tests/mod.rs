//! Tests of collections. Here the basics and the helpers the topic
//! files share; each topic file holds one part of the spec.

use super::count::LEAVES_COUNTED;
use super::walk::Spans;
use super::write::{apply_unchecked, index_keys, stale_indexes};
use super::*;
use crate::Upserted;
use crate::database::Database;
use crate::document::{encode_document, with_id};
use crate::explain::EntryCount;
use crate::index::Index;
use crate::query::{Condition, Op, QueryPlan};
use crate::query::{Sort, SortOrder};
use crate::testing::XorShift;
use crate::testing::fresh_db;
use serde::{Deserialize, Serialize};

mod aggregates;
mod and_or_not;
mod cuts;
mod datetime;
mod find_one;
mod indexes;
mod projection;
mod sort;
mod unique;
mod updates;

/// `{"v": value}`: a document holding a value, as every document is
/// an object (SPEC §95).
fn v(value: Document) -> Document {
    Document::Object([("v".to_string(), value)].into_iter().collect())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct User {
    name: String,
    age: i64,
}

#[test]
fn the_primary_key_index_is_neither_created_nor_dropped() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<User>("users");
    for fields in ["_id", "_id.x"] {
        let create = users.ensure_index(fields).unwrap_err().to_string();
        let drop = users.drop_index(fields).unwrap_err().to_string();
        assert!(create.contains("primary key"), "{create}");
        assert_eq!(create, drop);
    }
    assert!(!users.drop_index("age").unwrap());
}

#[test]
fn typed_collection_insert_then_get_roundtrips() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<User>("users");

    let id = users
        .insert(User {
            name: "Ada".to_string(),
            age: 30,
        })
        .unwrap();

    let found = users.get(&id).unwrap();
    assert_eq!(
        found,
        Some(User {
            name: "Ada".to_string(),
            age: 30,
        })
    );
}

#[test]
fn typed_collection_find_filters_by_field() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<User>("users");

    users
        .insert(User {
            name: "Young".to_string(),
            age: 10,
        })
        .unwrap();
    users
        .insert(User {
            name: "Old".to_string(),
            age: 30,
        })
        .unwrap();

    let filter = Filter {
        conditions: vec![crate::query::Condition::Compare {
            field: "age".to_string(),
            op: crate::query::Op::Gte,
            value: Document::Int(18),
        }],
        ..Default::default()
    };

    let found = users.find(filter).unwrap();
    assert_eq!(
        found,
        vec![User {
            name: "Old".to_string(),
            age: 30
        }]
    );
}

#[test]
fn insert_then_get_roundtrips() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<Document>("users");

    let ada = v(Document::String("Ada".to_string()));
    let id = users.insert(ada.clone()).unwrap();

    let found = users.get(&id).unwrap();
    assert_eq!(found, Some(with_id(ada, id)));
}

#[test]
fn get_on_unknown_id_is_none() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<Document>("users");

    assert_eq!(users.get(&DocId([9; 16])).unwrap(), None);
}

#[test]
fn get_on_never_touched_collection_is_none() {
    let (_dir, db) = fresh_db();
    let ghosts = db.collection::<Document>("ghosts");

    assert_eq!(ghosts.get(&DocId([1; 16])).unwrap(), None);
}

#[test]
fn update_replaces_content() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<Document>("users");

    let id = users.insert(v(Document::Int(1))).unwrap();
    assert!(users.update(&id, v(Document::Int(2))).unwrap());

    assert_eq!(
        users.get(&id).unwrap(),
        Some(with_id(v(Document::Int(2)), id))
    );
}

#[test]
fn update_on_unknown_id_returns_false() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<Document>("users");

    assert!(!users.update(&DocId([9; 16]), v(Document::Int(1))).unwrap());
}

#[test]
fn delete_removes_it_from_get_and_find() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<Document>("users");

    let id = users.insert(v(Document::Int(1))).unwrap();
    assert!(users.delete(&id).unwrap());

    assert_eq!(users.get(&id).unwrap(), None);
    assert_eq!(users.find(Filter::default()).unwrap(), Vec::new());
    assert!(!users.delete(&id).unwrap(), "second delete finds nothing");
}

#[test]
fn find_filters_by_field() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<Document>("users");

    let mut young = crate::Map::new();
    young.insert("age".to_string(), Document::Int(10));
    let mut old = crate::Map::new();
    old.insert("age".to_string(), Document::Int(30));

    users.insert(Document::Object(young)).unwrap();
    users.insert(Document::Object(old)).unwrap();

    let filter = Filter {
        conditions: vec![crate::query::Condition::Compare {
            field: "age".to_string(),
            op: crate::query::Op::Gte,
            value: Document::Int(18),
        }],
        ..Default::default()
    };

    let found = users.find(filter).unwrap();
    assert_eq!(found.len(), 1);
}

#[test]
fn insert_then_get_includes_the_id_field() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<Document>("users");

    let mut fields = crate::Map::new();
    fields.insert("name".to_string(), Document::String("Ada".to_string()));
    let id = users.insert(Document::Object(fields)).unwrap();

    let Document::Object(found) = users.get(&id).unwrap().unwrap() else {
        panic!("expected an Object back");
    };
    assert_eq!(found.get("_id"), Some(&Document::Id(id)));
    assert_eq!(
        found.get("name"),
        Some(&Document::String("Ada".to_string()))
    );
}

#[test]
fn find_also_includes_the_id_field() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<Document>("users");

    let mut fields = crate::Map::new();
    fields.insert("name".to_string(), Document::String("Ada".to_string()));
    let id = users.insert(Document::Object(fields)).unwrap();

    let found = users.find(Filter::default()).unwrap();
    assert_eq!(found.len(), 1);
    let Document::Object(doc) = &found[0] else {
        panic!("expected an Object back");
    };
    assert_eq!(doc.get("_id"), Some(&Document::Id(id)));
}

/// An id in `_id` is the one an insert uses (SPEC §59, which changed
/// §18's rule of always making a new one); any other `_id` — a
/// string, the nil id — is still overwritten with the real one.
#[test]
fn insert_keeps_a_caller_supplied_id_and_overwrites_anything_else() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<Document>("users");
    let with_id = |id: Document| {
        let mut fields = crate::Map::new();
        fields.insert("_id".to_string(), id);
        fields.insert("name".to_string(), Document::String("Ada".to_string()));
        Document::Object(fields)
    };

    let chosen = DocId([0xFF; 16]);
    assert_eq!(users.insert(with_id(Document::Id(chosen))).unwrap(), chosen);
    let err = users.insert(with_id(Document::Id(chosen))).unwrap_err();
    assert!(
        matches!(err, crate::Error::DuplicateId { id, .. } if id == chosen),
        "{err:?}"
    );

    for other in [
        Document::String("mine".into()),
        Document::Id(DocId::NIL),
        Document::Null,
    ] {
        let id = users.insert(with_id(other.clone())).unwrap();
        assert!(id != chosen && id != DocId::NIL, "{other:?}");
        let Document::Object(found) = users.get(&id).unwrap().unwrap() else {
            panic!("expected an Object back");
        };
        assert_eq!(found.get("_id"), Some(&Document::Id(id)), "{other:?}");
    }
}

#[test]
fn update_also_refreshes_the_id_field() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<Document>("users");

    let mut fields = crate::Map::new();
    fields.insert("name".to_string(), Document::String("Ada".to_string()));
    let id = users.insert(Document::Object(fields)).unwrap();

    let mut updated = crate::Map::new();
    updated.insert("name".to_string(), Document::String("Grace".to_string()));
    assert!(users.update(&id, Document::Object(updated)).unwrap());

    let Document::Object(found) = users.get(&id).unwrap().unwrap() else {
        panic!("expected an Object back");
    };
    assert_eq!(found.get("_id"), Some(&Document::Id(id)));
    assert_eq!(
        found.get("name"),
        Some(&Document::String("Grace".to_string()))
    );
}

/// SPEC §95: every write refuses a document that isn't an object,
/// typed or untyped, alone or in a batch, and the batch rolls back.
#[test]
fn every_write_refuses_a_document_that_is_not_an_object() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<Document>("users");
    let ada = users.insert(v(Document::Int(1))).unwrap();
    let refused = |result: crate::Result<()>| {
        let Err(crate::Error::NotAnObject { collection }) = result else {
            panic!("expected NotAnObject, got {result:?}");
        };
        assert_eq!(collection, "users");
    };

    for value in [
        Document::Int(42),
        Document::String("Ada".into()),
        Document::Array(vec![Document::Int(1)]),
        Document::Null,
        Document::Bool(true),
    ] {
        refused(users.insert(value.clone()).map(drop));
        refused(users.update(&ada, value.clone()).map(drop));
        refused(users.upsert(Filter::new().eq("v", 2), value).map(drop));
    }
    refused(db.collection::<i64>("users").insert(7).map(drop));
    refused(db.collection::<Vec<i64>>("users").insert(vec![7]).map(drop));

    // A batch rolls back whole: the object before the number too.
    let mut batch = db.batch();
    batch.insert(&users, v(Document::Int(2))).unwrap();
    batch.insert(&users, Document::Int(3)).unwrap();
    refused(batch.commit().map(drop));
    // So does `update_many`, whose closure returned a number for one.
    users.insert(v(Document::Int(4))).unwrap();
    let changed = users.update_many(Filter::new(), |doc| {
        if crate::query::field_value(doc, "v") == Some(&Document::Int(4)) {
            *doc = Document::Int(4);
        } else {
            *doc = v(Document::Int(0));
        }
    });
    refused(changed.map(drop));

    let found = users.find(Filter::new().sort_asc("v")).unwrap();
    let values: Vec<Option<&Document>> = found
        .iter()
        .map(|doc| crate::query::field_value(doc, "v"))
        .collect();
    assert_eq!(values, [Some(&Document::Int(1)), Some(&Document::Int(4))]);
}

#[test]
fn two_collections_stay_independent() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<Document>("users");
    let posts = db.collection::<Document>("posts");

    let id = users
        .insert(v(Document::String("Ada".to_string())))
        .unwrap();

    assert_eq!(posts.get(&id).unwrap(), None);
}

/// The point of SPEC §20: small documents share pages. 1000
/// small documents used to take 1000 data pages (~8 MB); packed they
/// take a handful.
#[test]
fn small_documents_are_packed_into_few_pages() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.trunkdb");
    let db = Database::open(&path).unwrap();
    let pings = db.collection::<Document>("pings");

    let ops: Vec<WriteOp> = (0..1000)
        .map(|i| {
            let doc = Document::Object(
                [
                    ("lat".to_string(), Document::Int(i)),
                    ("lon".to_string(), Document::Int(-i)),
                ]
                .into_iter()
                .collect(),
            );
            WriteOp::Insert("pings".to_string(), db.id_gen().generate(), doc)
        })
        .collect();
    db.write_batch(ops).unwrap();

    let pages = std::fs::metadata(&path).unwrap().len() / crate::storage::PAGE_SIZE as u64;
    assert!(pages < 30, "1000 small documents took {pages} pages");
    assert_eq!(pings.find(Filter::default()).unwrap().len(), 1000);
}

/// Growing documents until they no longer fit beside their neighbors
/// moves them to other pages; the index must follow, across a reopen.
#[test]
fn documents_that_outgrow_their_page_stay_reachable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.trunkdb");
    let ids: Vec<DocId> = {
        let db = Database::open(&path).unwrap();
        let docs = db.collection::<Document>("docs");
        let ids: Vec<DocId> = (0..20)
            .map(|_| docs.insert(v(Document::Binary(vec![1; 100]))).unwrap())
            .collect();
        for (n, id) in ids.iter().enumerate() {
            assert!(
                docs.update(id, v(Document::Binary(vec![n as u8; 3000])))
                    .unwrap()
            );
        }
        ids
    };

    let db = Database::open(&path).unwrap();
    let docs = db.collection::<Document>("docs");
    for (n, id) in ids.iter().enumerate() {
        assert_eq!(
            docs.get(id).unwrap(),
            Some(with_id(v(Document::Binary(vec![n as u8; 3000])), *id))
        );
    }
    assert_eq!(docs.find(Filter::default()).unwrap().len(), 20);

    // Deleting everything frees the data pages for reuse by new ones.
    let len_before = std::fs::metadata(&path).unwrap().len();
    for id in &ids {
        assert!(docs.delete(id).unwrap());
    }
    for _ in 0..20 {
        docs.insert(v(Document::Binary(vec![9; 3000]))).unwrap();
    }
    assert_eq!(std::fs::metadata(&path).unwrap().len(), len_before);
    assert_eq!(docs.find(Filter::default()).unwrap().len(), 20);
}

fn age_filter(op: crate::query::Op, age: i64) -> Filter {
    Filter {
        conditions: vec![crate::query::Condition::Compare {
            field: "age".to_string(),
            op,
            value: Document::Int(age),
        }],
        ..Filter::default()
    }
}

/// The sync workload prototype's blocker (SPEC §5.3): find by a field,
/// then update by id — after a restart, with no id kept from
/// `insert`. The struct carries its id (§59), the way that replaced
/// `find_with_ids` (§74).
#[test]
fn a_found_struct_carries_an_updatable_id_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.trunkdb");
    {
        let db = Database::open(&path).unwrap();
        let tasks = db.collection::<Todo>("tasks");
        for title in ["write", "test", "ship"] {
            tasks.insert(todo(title)).unwrap();
        }
    }

    let db = Database::open(&path).unwrap();
    let tasks = db.collection::<Todo>("tasks");
    let found = tasks
        .find(Filter::new().any_of([
            Condition::eq("title", "test"),
            Condition::eq("title", "ship"),
        ]))
        .unwrap();
    assert_eq!(found.len(), 2);
    for task in &found {
        assert_eq!(tasks.get(&task.id.unwrap()).unwrap().as_ref(), Some(task));
    }

    let mut ship = found.into_iter().find(|t| t.title == "ship").unwrap();
    ship.owner = Some(DocId([7; 16]));
    let id = ship.id.unwrap();
    assert!(tasks.update(&id, ship.clone()).unwrap());
    assert_eq!(tasks.get(&id).unwrap(), Some(ship));
}

/// SPEC §60: `indexes` says what each index is — its fields and
/// options — in creation order; field names come in every form a
/// caller holds them in.
#[test]
fn indexes_list_their_fields_and_options() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    let from_config: Vec<String> = vec!["status".into(), "created".into()];
    docs.ensure_index("age").unwrap();
    docs.ensure_index_with("email", IndexOptions::new().unique())
        .unwrap();
    docs.ensure_index_with(from_config.clone(), IndexOptions::new().sparse())
        .unwrap();
    docs.ensure_index(vec!["a", "b"]).unwrap();
    docs.ensure_index(&["c".to_string()][..]).unwrap();

    let listed: Vec<(String, Vec<String>, bool, bool)> = docs
        .indexes()
        .unwrap()
        .into_iter()
        .map(|index| {
            let fields = index.fields().to_vec();
            (index.name(), fields, index.is_unique(), index.is_sparse())
        })
        .collect();
    let owned = |fields: &[&str]| fields.iter().map(|f| f.to_string()).collect::<Vec<_>>();
    assert_eq!(
        listed,
        [
            ("age".to_string(), owned(&["age"]), false, false),
            ("email".to_string(), owned(&["email"]), true, false),
            ("(status, created)".to_string(), from_config, false, true),
            ("(a, b)".to_string(), owned(&["a", "b"]), false, false),
            ("c".to_string(), owned(&["c"]), false, false),
        ]
    );
    assert_eq!(
        docs.indexes().unwrap()[1].options(),
        IndexOptions::new().unique()
    );
}

/// `limit` takes a number or an `Option` of one, so a caller's
/// optional limit passes straight through.
#[test]
fn limit_takes_an_option() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    for n in 0..5 {
        docs.insert(v(Document::Int(n))).unwrap();
    }
    let found = |limit: Option<usize>| docs.find(Filter::new().limit(limit)).unwrap().len();
    assert_eq!(found(None), 5);
    assert_eq!(found(Some(2)), 2);
    assert_eq!(docs.find(Filter::new().limit(3)).unwrap().len(), 3);
}

/// SPEC §59: a struct carrying `#[serde(rename = "_id")] id:
/// Option<DocId>`, and a `DocId` field referring to another document.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct Todo {
    #[serde(rename = "_id")]
    id: Option<DocId>,
    title: String,
    owner: Option<DocId>,
}

fn todo(title: &str) -> Todo {
    Todo {
        id: None,
        title: title.to_string(),
        owner: None,
    }
}

/// SPEC §184.3: the typed `update_many` stores each match as the `T`
/// it became, so a field `T` doesn't declare is dropped, even by a
/// closure that changes nothing; `update_fields` keeps it.
#[test]
fn a_typed_update_many_drops_the_fields_its_type_lacks() {
    #[derive(Serialize, Deserialize)]
    struct Older {
        name: String,
        age: i64,
    }
    let (_dir, db) = fresh_db();
    let users = db.collection::<Document>("users");
    let doc = |text: &str| text.parse::<Document>().unwrap();
    let ada = users
        .insert(doc(r#"{"name": "Ada", "age": 36, "email": "a@x"}"#))
        .unwrap();
    let bob = users
        .insert(doc(r#"{"name": "Bob", "age": 41, "email": "b@x"}"#))
        .unwrap();
    let email = |id: &DocId| {
        let Document::Object(fields) = users.get(id).unwrap().unwrap() else {
            panic!("an object");
        };
        fields.get("email").cloned()
    };

    let older = db.collection::<Older>("users");
    let changed = older
        .update_many(Filter::new().eq("name", "Ada"), |_| {})
        .unwrap();
    assert_eq!(changed, 1, "written, though nothing was changed");
    assert_eq!(email(&ada), None, "dropped");

    older
        .update_fields(
            Filter::new().eq("name", "Bob"),
            &crate::query::Update::new().inc("age", 1),
        )
        .unwrap();
    assert_eq!(email(&bob), Some(Document::from("b@x")), "kept");
}

/// SPEC §74: a type without an id field, say from another crate, gets
/// one by being wrapped, its fields flattened into the wrapper's; the
/// stored document is the same, so the bare type still reads it.
#[test]
fn a_wrapper_gives_a_type_without_an_id_field_its_id() {
    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
    struct WithId {
        #[serde(rename = "_id")]
        id: Option<DocId>,
        #[serde(flatten)]
        user: User,
    }

    let (_dir, db) = fresh_db();
    let users = db.collection::<WithId>("users");
    users.ensure_index("age").unwrap();
    let ada = User {
        name: "Ada".to_string(),
        age: 36,
    };
    let id = users
        .insert(WithId {
            id: None,
            user: ada.clone(),
        })
        .unwrap();

    let by_age = Filter::new().eq("age", 36);
    let found = users.find(by_age.clone()).unwrap();
    let streamed: Vec<WithId> = users.cursor(by_age).unwrap().map(Result::unwrap).collect();
    let expected = WithId {
        id: Some(id),
        user: ada.clone(),
    };
    assert_eq!((found, streamed), (vec![expected.clone()], vec![expected]));
    let bare = db.collection::<User>("users").find(Filter::new()).unwrap();
    assert_eq!(bare, [ada]);
}

/// Every way of reading fills the id in; `None` on insert makes one,
/// `Some` is used; an update's id argument wins over the field.
#[test]
fn a_struct_carries_its_own_id() {
    let (_dir, db) = fresh_db();
    let tasks = db.collection::<Todo>("tasks");

    let made = tasks.insert(todo("made")).unwrap();
    let chosen = DocId([0xAB; 16]);
    let given = Todo {
        id: Some(chosen),
        ..todo("given")
    };
    assert_eq!(tasks.insert(given.clone()).unwrap(), chosen);
    assert!(matches!(
        tasks.insert(given.clone()).unwrap_err(),
        crate::Error::DuplicateId { id, .. } if id == chosen
    ));

    assert_eq!(tasks.get(&made).unwrap().unwrap().id, Some(made));
    assert_eq!(tasks.get(&chosen).unwrap(), Some(given));
    let everything = || Filter::new().sort_asc("title");
    let found: Vec<Option<DocId>> = tasks
        .find(everything())
        .unwrap()
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(found, [Some(chosen), Some(made)]);
    let one = tasks.find_one(Filter::new().eq("title", "made")).unwrap();
    assert_eq!(one.unwrap().id, Some(made));
    let streamed: Vec<Option<DocId>> = tasks
        .cursor(Filter::new())
        .unwrap()
        .map(|item| item.unwrap().id)
        .collect();
    assert_eq!(streamed.len(), 2);
    assert!(streamed.iter().all(Option::is_some));

    // The argument names the document; the field can't move it.
    let moved = Todo {
        id: Some(DocId([1; 16])),
        ..todo("renamed")
    };
    assert!(tasks.update(&made, moved).unwrap());
    assert_eq!(tasks.get(&made).unwrap().unwrap().id, Some(made));
    assert_eq!(tasks.get(&DocId([1; 16])).unwrap(), None);
}

/// `upsert` and a typed batch use a given id too, and a `DocId` field
/// is stored as an id, so a filter on it finds it.
#[test]
fn upserts_batches_and_references_keep_ids() {
    let (_dir, db) = fresh_db();
    let tasks = db.collection::<Todo>("tasks");

    let upserted = DocId([2; 16]);
    let outcome = tasks
        .upsert(
            Filter::new().eq("title", "nothing"),
            Todo {
                id: Some(upserted),
                ..todo("upserted")
            },
        )
        .unwrap();
    assert_eq!(outcome, Upserted::Inserted(upserted));

    let batched = DocId([3; 16]);
    let mut batch = db.batch();
    let id = batch
        .insert(
            &tasks,
            Todo {
                id: Some(batched),
                owner: Some(upserted),
                ..todo("batched")
            },
        )
        .unwrap();
    assert_eq!(id, batched);
    batch.commit().unwrap();

    // A filter on a reference: a scan here, with no index on it
    // (with one, it reads the index since SPEC §62).
    let by_owner = Filter::new().eq("owner", upserted);
    assert_eq!(
        tasks.explain(&by_owner).unwrap(),
        crate::query::QueryPlan::Scan
    );
    let found = tasks.find(by_owner).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(
        (found[0].id, found[0].owner),
        (Some(batched), Some(upserted))
    );
}

/// The other style: a plain `DocId` field, `DocId::NIL` meaning "make
/// one".
#[test]
fn a_plain_id_field_with_nil_for_new() {
    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Note {
        #[serde(rename = "_id")]
        id: DocId,
        text: String,
    }
    let (_dir, db) = fresh_db();
    let notes = db.collection::<Note>("notes");
    let id = notes
        .insert(Note {
            id: DocId::NIL,
            text: "hi".into(),
        })
        .unwrap();
    assert_ne!(id, DocId::NIL);
    assert_eq!(notes.get(&id).unwrap().unwrap().id, id);
}

/// Sort and limit act on the documents, and each id stays with its
/// own document through both — what `delete_many` and `update_many`
/// rely on to write the right ones.
#[test]
fn ids_stay_paired_through_sort_and_limit() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<User>("users");
    let mut ids = std::collections::HashMap::new();
    for (name, age) in [("Ada", 36), ("Grace", 45), ("Linus", 21), ("Ken", 30)] {
        let id = users
            .insert(User {
                name: name.to_string(),
                age,
            })
            .unwrap();
        ids.insert(name.to_string(), id);
    }

    let filter = Filter {
        sort: vec![crate::query::Sort {
            field: "age".to_string(),
            order: crate::query::SortOrder::Desc,
        }],
        limit: Some(3),
        ..age_filter(crate::query::Op::Gte, 25)
    };
    let found = users.find_with_ids(filter).unwrap();

    let names: Vec<&str> = found.iter().map(|(_id, u)| u.name.as_str()).collect();
    assert_eq!(names, ["Grace", "Ada", "Ken"]);
    for (id, user) in &found {
        assert_eq!(ids[&user.name], *id);
    }
}

/// SPEC §71: a tool that only speaks JSON text — inserts it, reads
/// it, edits it, writes it back — through `Collection<Document>`.
#[test]
fn documents_go_in_and_out_as_json_text() {
    let (_dir, db) = fresh_db();
    let people = db.collection::<Document>("people");

    let new: Document = r#"{"name":"Ann","born":{"$date":"1990-05-01T12:00:00Z"}}"#
        .parse()
        .unwrap();
    let id = people.insert(new).unwrap();
    let text = people.get(&id).unwrap().unwrap().to_string();
    assert_eq!(
        text,
        format!(
            r#"{{"_id":{{"$id":"{id}"}},"name":"Ann","born":{{"$date":"1990-05-01T12:00:00Z"}}}}"#
        )
    );

    let edited = text.replace("Ann", "Bea").parse().unwrap();
    assert!(people.update(&id, edited).unwrap());
    let found = people.find(Filter::new().eq("name", "Bea")).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].to_string(), text.replace("Ann", "Bea"));

    // The id in the text is kept on insert, as any `_id` holding an id is (§59).
    let other = DocId([0x42; 16]);
    let copy: Document = text
        .replace(&id.to_string(), &other.to_string())
        .parse()
        .unwrap();
    assert_eq!(people.insert(copy.clone()).unwrap(), other);
    // An update takes its id from its argument; the text's `_id` is ignored.
    assert!(people.update(&id, copy).unwrap());
    let got = people.get(&id).unwrap().unwrap();
    assert_eq!(got.to_string(), text);
}

/// A search field (SPEC §5.3): a case-insensitive
/// substring search, combined with other conditions, on the typed path.
#[test]
fn typed_find_with_a_case_insensitive_contains() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<User>("users");
    for (name, age) in [("Ada Lovelace", 36), ("ada", 12), ("Grace Hopper", 45)] {
        users
            .insert(User {
                name: name.to_string(),
                age,
            })
            .unwrap();
    }

    let mut filter = age_filter(crate::query::Op::Gt, 18);
    filter.conditions.push(crate::query::Condition::Compare {
        field: "name".to_string(),
        op: crate::query::Op::Contains,
        value: Document::String("ADA".to_string()),
    });
    let found = users.find(filter).unwrap();

    assert_eq!(
        found,
        [User {
            name: "Ada Lovelace".to_string(),
            age: 36
        }]
    );
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Article {
    title: String,
    body: String,
}

/// The large-document workload (SPEC §5.2, §26): articles whose body is
/// far larger than a page, stored, found by content, rewritten, and read back after a
/// reopen.
#[test]
fn documents_larger_than_a_page_work_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.trunkdb");
    let article = |title: &str, sentence: &str, times| Article {
        title: title.to_string(),
        body: sentence.repeat(times),
    };
    let id = {
        let db = Database::open(&path).unwrap();
        let articles = db.collection::<Article>("articles");
        articles
            .insert(article("short", "A quiet morning. ", 3))
            .unwrap();
        let id = articles
            .insert(article("long", "The storm kept on. ", 5_000))
            .unwrap();
        assert!(
            articles
                .update(&id, article("long", "The storm broke at last. ", 8_000))
                .unwrap()
        );
        id
    };

    let db = Database::open(&path).unwrap();
    let articles = db.collection::<Article>("articles");
    let expected = article("long", "The storm broke at last. ", 8_000);
    assert_eq!(articles.get(&id).unwrap(), Some(expected.clone()));
    let found = articles
        .find(Filter {
            conditions: vec![crate::query::Condition::Compare {
                field: "body".to_string(),
                op: crate::query::Op::Contains,
                value: Document::String("STORM BROKE".to_string()),
            }],
            ..Filter::default()
        })
        .unwrap();
    assert_eq!(found, vec![expected]);
}

// --- Helpers the topic files share ---

/// A random condition up to `depth` levels deep: comparisons with
/// every operator on the indexed `v` and `w`, the unindexed `pad` and
/// a field no document has — combined by ANDs, ORs (empty ones too)
/// and NOTs.
fn random_condition(rng: &mut XorShift, depth: usize) -> Condition {
    if depth == 0 || rng.below(3) == 0 {
        #[cfg(feature = "regex")]
        if rng.below(8) == 0 {
            let field = ["v", "v", "v[*]", "pad", "nowhere"][rng.below(5)];
            return Condition::regex(field, random_regex(rng));
        }
        let ops = [
            Op::Eq,
            Op::Ne,
            Op::Lt,
            Op::Lte,
            Op::Gt,
            Op::Gte,
            Op::Contains,
            Op::StartsWith,
        ];
        let op = ops[rng.below(ops.len())].clone();
        return match rng.below(6) {
            0..=2 => cond("v", op, random_value(rng)),
            3 => cond("w", op, Document::Int(rng.below(4) as i64)),
            4 => cond("pad", op, Document::String(String::new())),
            _ => cond("nowhere", op, random_value(rng)),
        };
    }
    let children = (0..rng.below(4))
        .map(|_| random_condition(rng, depth - 1))
        .collect::<Vec<_>>();
    match rng.below(5) {
        0 => Condition::All(children),
        1 => !random_condition(rng, depth - 1),
        _ => Condition::Any(children),
    }
}

/// Every random nested filter must find, count and stream exactly
/// what checking every document finds — whichever plan it runs,
/// also when it names documents by id.
fn assert_nested_filters_agree_with_a_scan(docs: &Collection<Document>, rng: &mut XorShift) {
    assert_consistent(docs.db());
    let mut all = docs.find_with_ids(Filter::default()).unwrap();
    all.sort_by_key(|(id, _doc)| *id);
    let mut plans = std::collections::HashSet::new();
    for _ in 0..400 {
        let mut f = Filter::new();
        for _ in 0..1 + rng.below(2) {
            f = f.and(random_condition(rng, 3));
        }
        if rng.below(4) == 0 {
            // Documents named by id (SPEC §61): one or an OR of a few,
            // now and then one that doesn't exist.
            let named = (0..1 + rng.below(3))
                .map(|_| match rng.below(5) {
                    0 => Condition::eq("_id", DocId([0xAB; 16])),
                    _ => Condition::eq("_id", all[rng.below(all.len())].0),
                })
                .collect::<Vec<_>>();
            f = f.and(Condition::Any(named));
        }
        if rng.below(3) == 0 {
            f = f.sort_desc("v").limit([1, 5, 50][rng.below(3)]);
            // Ties in `v` by `w` (SPEC §47): the index on `v` gives
            // the order of `v` only, `w` is sorted in memory.
            if rng.below(2) == 0 {
                f = f.then_by("w", [SortOrder::Asc, SortOrder::Desc][rng.below(2)]);
            }
        }
        let ids = |found: Vec<(DocId, Document)>| -> Vec<DocId> {
            found.into_iter().map(|(id, _)| id).collect()
        };
        let mut expected = ids(f.apply_to(all.clone(), |(_, doc)| doc));
        let plan = explained(docs, &f);
        let mut found = ids(docs.find_with_ids(f.clone()).unwrap());
        let streamed = docs
            .cursor(f.clone())
            .unwrap()
            .collect::<crate::Result<Vec<_>>>();
        // A cursor hands out documents, each with its `_id` (SPEC §74).
        let mut streamed: Vec<DocId> = streamed
            .unwrap()
            .iter()
            .map(|doc| match doc {
                Document::Object(fields) => match fields["_id"] {
                    Document::Id(id) => id,
                    _ => panic!("no id in {doc}"),
                },
                _ => panic!("not an object: {doc}"),
            })
            .collect();
        if f.sort.is_empty() {
            expected.sort();
            found.sort();
            streamed.sort();
        }
        assert_eq!(found, expected, "{f:?} via {plan:?}");
        assert_eq!(streamed, expected, "cursor: {f:?} via {plan:?}");
        let counted = docs
            .count(Filter {
                sort: Vec::new(),
                ..f.clone()
            })
            .unwrap();
        assert_eq!(counted, expected.len(), "count: {f:?} via {plan:?}");
        plans.insert(std::mem::discriminant(&plan));
    }
    assert_eq!(
        plans.len(),
        5,
        "scan, index, union, order and by id all ran"
    );
}

fn user(name: &str, age: i64) -> User {
    User {
        name: name.to_string(),
        age,
    }
}

fn by_age(order: SortOrder) -> Filter {
    Filter {
        sort: vec![Sort {
            field: "age".to_string(),
            order,
        }],
        ..Filter::default()
    }
}

fn object(pairs: Vec<(&str, Document)>) -> Document {
    Document::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn cond(field: &str, op: Op, value: Document) -> Condition {
    Condition::Compare {
        field: field.to_string(),
        op,
        value,
    }
}

fn filter(conditions: Vec<Condition>) -> Filter {
    Filter {
        conditions,
        ..Filter::default()
    }
}

/// A value of a type an index handles, or doesn't — duplicates are
/// likely, and so are values that share an encoding (large ints
/// rounding to one f64, long strings agreeing past the key's cut).
/// A regex for the random conditions (SPEC §169): with a fixed
/// prefix an index reads as a range ("a", "ab", "x", the 1,200 x's a
/// key cuts), and without one; anchored at the end, ignoring case,
/// a class, the empty pattern.
#[cfg(feature = "regex")]
fn random_regex(rng: &mut XorShift) -> crate::query::Regex {
    let patterns = [
        "^a", "^ab", "^x", "^xxxx", "^x+b$", "^a\\x00", "^$", "^b|^a", "a$", "b", "(?i)^b",
        "^[ab]", "", "x{1200}a",
    ];
    crate::query::Regex::new(patterns[rng.below(patterns.len())]).unwrap()
}

fn random_value(rng: &mut XorShift) -> Document {
    let big = 1i64 << 53;
    match rng.below(14) {
        0 | 1 => Document::Int(rng.below(11) as i64 - 5),
        2 => Document::Int(big + rng.below(4) as i64),
        3 => Document::Float((rng.below(21) as f64 - 10.0) / 2.0),
        4 => [Document::Float(-0.0), Document::Float(f64::NAN)][rng.below(2)].clone(),
        5 | 6 => Document::String(["", "a", "ab", "b", "B", "a\0", "x"][rng.below(7)].to_string()),
        7 => Document::String("x".repeat(1200) + ["a", "b", ""][rng.below(3)]),
        8 => Document::Bool(rng.below(2) == 1),
        9 => [Document::Null, Document::Array(vec![Document::Int(1)])][rng.below(2)].clone(),
        // A few ids, which compare, sort and are indexed (SPEC §62).
        10 => Document::Id(DocId([1 + rng.below(3) as u8; 16])),
        // Binary, ordered by its bytes and indexed (SPEC §166): zero
        // bytes, which the key escapes, and long ones the key cuts.
        12 => {
            Document::Binary([&[][..], &[0], &[0, 0], &[1], &[0xFF], b"a"][rng.below(6)].to_vec())
        }
        13 => Document::Binary([vec![0; 1200], vec![1]].concat()[..1200 + rng.below(2)].to_vec()),
        // A few date-times, before and after 1970 (SPEC §69), and a
        // few dates and times of day (§173).
        _ => match rng.below(3) {
            // Two of each, so values still collide as often as the
            // tests of unique indexes and updates need.
            0 => {
                Document::Date(crate::Date::from_days_since_epoch(rng.below(2) as i64 - 1).unwrap())
            }
            1 => Document::Time(
                crate::Time::from_nanos_since_midnight([0, 86_399_999_999_999][rng.below(2)])
                    .unwrap(),
            ),
            _ => Document::DateTime(
                crate::DateTime::from_unix(rng.below(5) as i64 - 2, [0, 999_999_999][rng.below(2)])
                    .unwrap(),
            ),
        },
    }
}

fn random_document(rng: &mut XorShift) -> Document {
    let mut fields = vec![("w", Document::Int(rng.below(3) as i64))];
    if rng.below(8) != 0 {
        fields.push(("v", random_value(rng)));
    }
    // Padding of very different sizes, so an update often no longer
    // fits its page and moves the document (SPEC §20.3) — its index
    // entries must follow even when the value didn't change. Now and
    // then large enough for overflow pages.
    let pad = [0, 0, 1500, 3000, 4500, 9000][rng.below(6)];
    fields.push(("pad", Document::String("y".repeat(pad))));
    object(fields)
}

/// Conditions on the indexed `path`, sometimes with one on `w`.
fn random_filter(rng: &mut XorShift, path: &str) -> Filter {
    let ops = [Op::Eq, Op::Lt, Op::Lte, Op::Gt, Op::Gte];
    let mut conditions = vec![cond(path, ops[rng.below(5)].clone(), random_value(rng))];
    match rng.below(3) {
        0 => conditions.push(cond(path, ops[rng.below(5)].clone(), random_value(rng))),
        1 => conditions.push(cond("w", Op::Eq, Document::Int(rng.below(3) as i64))),
        _ => {}
    }
    filter(conditions)
}

fn sorted_ids(results: Vec<(DocId, Document)>) -> Vec<DocId> {
    let mut ids: Vec<DocId> = results.into_iter().map(|(id, _)| id).collect();
    ids.sort();
    ids
}

/// The whole file checks out (SPEC §39): no leaked or doubly used
/// page, every index agreeing with the documents. Then it's compacted
/// (SPEC §41) — keeping every document, and checking out again — so
/// whatever the test does next runs on a rebuilt file.
fn assert_consistent(db: &Database) {
    let report = db.check().unwrap();
    assert!(report.is_ok(), "{:#?}", report.problems);
    assert_free_space_is_true(db);

    let everything = |db: &Database| {
        let mut names = db.collections().unwrap();
        names.sort();
        names
            .into_iter()
            .map(|name| {
                let docs = db.collection::<Document>(&name);
                let mut all = docs.find_with_ids(Filter::default()).unwrap();
                all.sort_by_key(|(id, _)| *id);
                // Debug text: a NaN isn't equal to itself.
                format!("{name} {:?} {all:?}", docs.index_names().unwrap())
            })
            .collect::<Vec<_>>()
    };
    let before = everything(db);
    let compacted = db.compact().unwrap();
    assert!(compacted.pages_after <= compacted.pages_before);
    assert_eq!(everything(db), before);
    let report = db.check().unwrap();
    assert!(report.is_ok(), "after compacting: {:#?}", report.problems);
}

/// Every random filter must find through the index exactly what it
/// finds by checking every document.
fn assert_index_agrees_with_scan(docs: &Collection<Document>, rng: &mut XorShift, path: &str) {
    assert_consistent(docs.db());
    let all = docs.find_with_ids(Filter::default()).unwrap();
    for _ in 0..300 {
        let f = random_filter(rng, path);
        let expected = sorted_ids(f.apply_to(all.clone(), |(_, doc)| doc));
        let plan = explained(docs, &f);
        let found = sorted_ids(docs.find_with_ids(f.clone()).unwrap());
        assert_eq!(found, expected, "{f:?} via {plan:?}");
    }
}

const SPARSE: IndexOptions = IndexOptions {
    unique: false,
    sparse: true,
};

/// `explain`'s plan for `f`, after checking `explain_run` against it
/// and against `find` (SPEC §94.7): the same plan; as many returned
/// and as many documents read as `find` returns and reads; counts
/// that agree with each other and with the plan; and every document
/// found within the bounds of a read.
fn explained(docs: &Collection<Document>, f: &Filter) -> QueryPlan {
    let plan = docs.explain(f).unwrap();
    let run = docs.explain_run(f.clone()).unwrap();
    assert_eq!(run.plan, plan, "{f:?}");
    let mut found = Vec::new();
    let read = records_read(|| found = docs.find_with_ids(f.clone()).unwrap());
    let stats = run.stats.clone();
    assert_eq!(stats.documents_returned, found.len() as u64, "{f:?}");
    assert_eq!(stats.documents_examined, read as u64, "{f:?} via {plan:?}");
    let entries: u64 = run.reads.iter().map(|read| read.entries).sum();
    assert_eq!(stats.keys_examined, entries);
    assert!(
        stats.keys_examined >= stats.documents_examined,
        "{f:?}: {run:?}"
    );
    // Where entries decide (SPEC §100), a match need not be read;
    // elsewhere every match was.
    match run.reads.iter().any(|read| read.covering) {
        true => assert!(stats.documents_matched <= stats.keys_examined),
        false => assert!(
            stats.documents_matched <= stats.documents_examined,
            "{f:?}: {run:?}"
        ),
    }
    // A read covers exactly where its index holds what the filter
    // names, and isn't multikey.
    let indexes = docs.indexes().unwrap();
    for (i, read) in run.reads.iter().enumerate() {
        let fields: Vec<String> = match read.index.as_str() {
            "_id" => Vec::new(),
            name => indexes
                .iter()
                .find(|index| index.name() == name)
                .map(|index| index.fields().to_vec())
                .unwrap(),
        };
        let multi = fields.iter().any(|field| crate::query::is_multi(field));
        let covers = !multi && f.covered_by(&fields, false).is_some();
        // Ids looked up beside a range make a union of several
        // indexes too (SPEC §164), the primary among them.
        let with_ids = matches!(
            &run.plan,
            QueryPlan::IndexUnion { fields, .. } if fields[0] == "_id"
        );
        let several = with_ids
            || run
                .reads
                .iter()
                .any(|other| other.index != read.index && other.index != "_id");
        // The scan after a walk for what its index lacks reads
        // documents (§34.2).
        let after_walk = i > 0 && read.index == "_id";
        assert_eq!(
            read.covering,
            covers && !several && !after_walk,
            "{f:?}: {run:?}"
        );
    }
    let unlimited = Filter {
        skip: 0,
        limit: None,
        ..f.clone()
    };
    let matching = docs.count(unlimited).unwrap() as u64;
    match plan {
        // Stops once it has skip plus limit.
        QueryPlan::IndexOrder { .. } | QueryPlan::IndexUnion { in_order: true, .. } => {
            let wanted = match f.limit.unwrap() {
                0 => 0, // reads nothing at all
                limit => (f.skip as u64).saturating_add(limit as u64),
            };
            assert!(
                stats.documents_matched >= matching.min(wanted),
                "{f:?}: {run:?}"
            );
            assert!(stats.documents_matched <= matching, "{f:?}: {run:?}");
        }
        _ => assert_eq!(stats.documents_matched, matching, "{f:?}: {run:?}"),
    }
    let names: Vec<&str> = run.reads.iter().map(|read| read.index.as_str()).collect();
    match &plan {
        QueryPlan::Scan | QueryPlan::ById => {
            assert!(names.is_empty() || names == ["_id"], "{f:?}: {run:?}");
        }
        QueryPlan::Index { field } => assert_eq!(names, [field.as_str()]),
        QueryPlan::IndexUnion { in_order: true, .. } if f.limit == Some(0) => {
            assert!(names.is_empty())
        }
        QueryPlan::IndexUnion { fields, .. } => assert_eq!(names, *fields),
        QueryPlan::IndexOrder { .. } if f.limit == Some(0) => assert!(names.is_empty()),
        QueryPlan::IndexOrder { field } => {
            assert_eq!(names[0], field);
            assert!(
                names[1..].is_empty() || names[1..] == ["_id"],
                "{f:?}: {run:?}"
            );
            let descending = f.sort[0].order == SortOrder::Desc;
            assert_eq!(run.reads[0].backward, descending);
        }
    }
    if !matches!(plan, QueryPlan::Scan | QueryPlan::IndexOrder { .. }) {
        for read in &run.reads {
            assert!(!read.bounds.is_empty(), "{f:?}: {run:?}");
        }
    }
    // A document found is in some read: it meets all of its bounds.
    for (_id, doc) in &found {
        assert!(
            run.reads
                .iter()
                .any(|read| read.bounds.iter().all(|bound| bound.matches(doc))),
            "{doc:?} outside every read of {run:?}"
        );
    }
    match &run.sort {
        None => assert!(f.sort.is_empty()),
        Some(sort) => {
            assert_eq!(sort.from_index + sort.in_memory, f.sort.len());
            let ordered = matches!(
                plan,
                QueryPlan::IndexOrder { .. } | QueryPlan::IndexUnion { in_order: true, .. }
            );
            assert_eq!(sort.from_index > 0, ordered, "{f:?}: {run:?}");
        }
    }
    plan
}

/// The ids `docs` finds for `f` through the typed path, from `find`
/// and from `cursor`, which must agree.
fn typed_ids(docs: &Collection<Document>, f: &Filter) -> Vec<DocId> {
    let typed = docs.db().collection::<IdOnly>(&docs.name);
    let found: Vec<DocId> = typed
        .find(f.clone())
        .unwrap()
        .iter()
        .map(|d| d.id)
        .collect();
    let streamed: Vec<DocId> = typed
        .cursor(f.clone())
        .unwrap()
        .map(|d| d.unwrap().id)
        .collect();
    if !f.sort.is_empty() || (f.limit.is_none() && f.skip == 0) {
        let sorted = |mut ids: Vec<DocId>| {
            if f.sort.is_empty() {
                ids.sort();
            }
            ids
        };
        assert_eq!(sorted(streamed), sorted(found.clone()), "{f:?}");
    }
    found
}

/// SPEC §106: `f` with a random selection hands out, on every route,
/// the documents `f` finds, each cut down to the selection — and reads
/// no document more, the ones answered from their entries fewer.
/// Half the time the selection is one index's fields, which its
/// entries may answer (§106.5). A third of the time some paths are
/// excepted too (§143), and half of those without the selection.
/// Returns how many results came from entries.
fn selection_agrees(docs: &Collection<Document>, f: &Filter, rng: &mut XorShift) -> u64 {
    let whole = docs.find_with_ids(f.clone()).unwrap();
    let indexes = docs.indexes().unwrap();
    let chosen: Vec<String> = match rng.below(2) {
        0 if !indexes.is_empty() => {
            let index = &indexes[rng.below(indexes.len())];
            let mut fields: Vec<String> = index.fields().to_vec();
            fields.truncate(1 + rng.below(fields.len()));
            fields
        }
        _ => {
            let pool = selectable(docs, &whole);
            (0..rng.below(4))
                .map(|_| pool[rng.below(pool.len())].clone())
                .collect()
        }
    };
    // Drawn apart, so the corpora draw what they drew before §143.
    let mut apart = XorShift((rng.0 ^ 0x9E37_79B9_7F4A_7C15).rotate_left(29) | 1);
    let pool = selectable(docs, &whole);
    let except: Vec<String> = match apart.below(3) {
        0 if !pool.is_empty() => (0..1 + apart.below(2))
            .map(|_| pool[apart.below(pool.len())].clone())
            .collect(),
        _ => Vec::new(),
    };
    let selects = except.is_empty() || apart.below(2) == 0;
    let (mut s, selection) = match selects {
        true => (
            f.clone().select(chosen.iter().map(String::as_str)),
            crate::select::Selection::new(chosen.iter().map(String::as_str)),
        ),
        false => (f.clone(), crate::select::Selection::everything()),
    };
    s = s.except(except.iter().map(String::as_str));
    let selection = selection.excepting(except.iter().map(String::as_str));
    let cut = |doc: Document| selection.project(doc).to_string();
    let expected: Vec<String> = whole.iter().map(|(_, doc)| cut(doc.clone())).collect();
    let text =
        |found: Vec<Document>| -> Vec<String> { found.iter().map(Document::to_string).collect() };
    let in_order = !f.sort.is_empty() || (f.limit.is_none() && f.skip == 0);
    let sorted = |mut texts: Vec<String>| {
        if f.sort.is_empty() {
            texts.sort();
        }
        texts
    };
    assert_eq!(text(docs.find(s.clone()).unwrap()), expected, "{s:?}");
    let view = docs
        .db()
        .snapshot()
        .unwrap()
        .collection::<Document>(&docs.name);
    assert_eq!(text(view.find(s.clone()).unwrap()), expected, "view: {s:?}");
    let streamed: Vec<Document> = docs
        .cursor(s.clone())
        .unwrap()
        .map(Result::unwrap)
        .collect();
    if in_order {
        assert_eq!(
            sorted(text(streamed)),
            sorted(expected.clone()),
            "cursor: {s:?}"
        );
    } else {
        assert_eq!(streamed.len(), expected.len(), "cursor: {s:?}");
    }
    let one = docs.find_one(s.clone()).unwrap().map(|doc| doc.to_string());
    match f.sort.is_empty() {
        false => assert_eq!(one.as_ref(), expected.first(), "find_one: {s:?}"),
        true => assert_eq!(one.is_some(), !expected.is_empty(), "find_one: {s:?}"),
    }
    // Typed: `T` given the selection's fields, and nothing else.
    let typed = docs.db().collection::<Keys>(&docs.name);
    let keys = |doc: &str| -> Vec<String> {
        match doc.parse::<Document>().unwrap() {
            Document::Object(map) => {
                let mut keys: Vec<String> = map.keys().cloned().collect();
                keys.sort();
                keys
            }
            _ => Vec::new(),
        }
    };
    let typed_keys: Vec<Vec<String>> = (typed.find(s.clone()).unwrap().into_iter())
        .map(|Keys(map)| map.into_keys().collect())
        .collect();
    let expected_keys: Vec<Vec<String>> = expected.iter().map(|doc| keys(doc)).collect();
    assert_eq!(typed_keys, expected_keys, "typed: {s:?}");
    let typed_ids: Vec<DocId> = (docs.db().collection::<IdOnly>(&docs.name))
        .find(s.clone())
        .unwrap()
        .iter()
        .map(|d| d.id)
        .collect();
    let ids: Vec<DocId> = whole.iter().map(|(id, _)| *id).collect();
    assert_eq!(typed_ids, ids, "typed ids: {s:?}");
    // The same reads, less the documents answered from their entries.
    let (plain, selected) = (
        docs.explain_run(f.clone()).unwrap(),
        docs.explain_run(s.clone()).unwrap(),
    );
    assert_eq!(selected.select.as_deref(), selects.then_some(&chosen[..]));
    assert_eq!(selected.except, except);
    assert_eq!(selected.stats.documents_returned, expected.len() as u64);
    assert_eq!(plain.stats.from_entries, 0);
    assert_eq!(
        selected.stats.documents_examined + selected.stats.from_entries,
        plain.stats.documents_examined,
        "{s:?}: {selected:?} against {plain:?}"
    );
    selected.stats.from_entries
}

/// SPEC §107: a random aggregate of what `f` finds gives, through
/// `aggregate` and a `View`, what `Aggregate::apply` gives on `find`'s
/// documents; half the time on one index's fields, which its entries
/// may answer (§107.5). Returns how many matches came from entries.
fn aggregate_agrees(docs: &Collection<Document>, f: &Filter, rng: &mut XorShift) -> u64 {
    use crate::query::Aggregate;
    let whole = docs.find_with_ids(f.clone()).unwrap();
    let indexes = docs.indexes().unwrap();
    let pool: Vec<String> = match rng.below(2) {
        0 if !indexes.is_empty() => {
            let index = &indexes[rng.below(indexes.len())];
            index.fields().to_vec()
        }
        _ => selectable(docs, &whole),
    };
    let pool: Vec<String> = (pool.into_iter())
        .filter(|p| !p.is_empty() && !p.contains("[*]") && p != "_id")
        .collect();
    if pool.is_empty() {
        return 0;
    }
    let pick = |rng: &mut XorShift| pool[rng.below(pool.len())].clone();
    let mut aggregate = Aggregate::new();
    let mut grouped: Vec<String> = Vec::new();
    for _ in 0..rng.below(3) {
        let path = pick(rng);
        let clash = |g: &String| {
            g == &path || g.starts_with(&format!("{path}.")) || path.starts_with(&format!("{g}."))
        };
        if !grouped.iter().any(clash) {
            aggregate = aggregate.group_by(path.clone());
            grouped.push(path);
        }
    }
    // A condition for a column (SPEC §126): on a path of the pool, so
    // half the time on the index's fields, against a value of a
    // document found or a random one; now and then negated, an OR of
    // two, or an `exists`, which no entry decides.
    let sample: Vec<Document> = whole.iter().map(|(_, d)| d.clone()).collect();
    let leaf = |rng: &mut XorShift| -> Condition {
        let path = pick(rng);
        let value = match (rng.below(2), sample.is_empty()) {
            (0, false) => {
                let doc = &sample[rng.below(sample.len())];
                crate::path::Path::new(&path).value_or_null(doc).clone()
            }
            _ => random_value(rng),
        };
        let ops = [Op::Eq, Op::Ne, Op::Lt, Op::Lte, Op::Gt, Op::Gte];
        cond(&path, ops[rng.below(ops.len())].clone(), value)
    };
    let condition = |rng: &mut XorShift| -> Condition {
        match rng.below(8) {
            0 => !leaf(rng),
            1 => Condition::any([leaf(rng), leaf(rng)]),
            2 => Condition::exists(pick(rng)),
            _ => leaf(rng),
        }
    };
    for c in 0..1 + rng.below(3) {
        let name = format!("c{c}");
        let mut column = match rng.below(5) {
            0 => aggregate.count(),
            1 => aggregate.sum(pick(rng)),
            2 => aggregate.min(pick(rng)),
            3 => aggregate.max(pick(rng)),
            _ => aggregate.avg(pick(rng)),
        };
        for _ in 0..[0, 0, 1, 1, 2][rng.below(5)] {
            column = column.filter(condition(rng));
        }
        aggregate = column.named(name);
    }
    if rng.below(3) == 0 {
        aggregate = aggregate.sort_desc("c0").limit(1 + rng.below(4));
    }
    let expected: Vec<String> = (aggregate.apply(whole.into_iter().map(|(_, d)| d)).unwrap())
        .iter()
        .map(Document::to_string)
        .collect();
    let text =
        |rows: Vec<Document>| -> Vec<String> { rows.iter().map(Document::to_string).collect() };
    let found = text(docs.aggregate(f.clone(), &aggregate).unwrap());
    assert_eq!(found, expected, "{f:?} {aggregate:?}");
    let view = docs
        .db()
        .snapshot()
        .unwrap()
        .collection::<Document>(&docs.name);
    assert_eq!(
        text(view.aggregate(f.clone(), &aggregate).unwrap()),
        expected
    );
    let run = docs.explain_aggregate(f.clone(), &aggregate).unwrap();
    assert_eq!(
        run.stats.documents_returned,
        expected.len() as u64,
        "{run:?}"
    );
    assert!(
        run.stats.from_entries <= run.stats.documents_matched,
        "{run:?}"
    );
    run.stats.from_entries
}

fn records_read(f: impl FnOnce()) -> usize {
    crate::data::RECORDS_READ.with(|n| n.set(0));
    f();
    crate::data::RECORDS_READ.with(|n| n.get())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Task {
    status: String,
    created: i64,
    tenant: String,
}

fn duplicate(result: crate::Result<impl std::fmt::Debug>) -> (DocId, DocId) {
    match result {
        Err(crate::Error::DuplicateValue { id, existing, .. }) => (id, existing),
        other => panic!("expected DuplicateValue, got {other:?}"),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Member {
    name: String,
    nick: Option<String>,
    team: Option<String>,
}

/// Entries in `collection`'s index named `name`.
fn entries(db: &Database, collection: &str, name: &str) -> usize {
    let state = db.read().unwrap();
    let index = state
        .catalog()
        .indexes(collection)
        .iter()
        .find(|index| index.name() == name)
        .unwrap()
        .clone();
    BTreeIndex::new(index.root)
        .scan(&state.snapshot().store)
        .unwrap()
        .len()
}

// --- Helpers the topic files share ---

/// Every collection's free-space map names only its own data pages,
/// with the room they really have (SPEC §75).
fn assert_free_space_is_true(db: &Database) {
    let state = db.state();
    for name in state.catalog().collection_names() {
        let Some(free) = state.catalog().free_space(name) else {
            continue;
        };
        let meta = *state.catalog().get(name).unwrap();
        let locs = BTreeIndex::new(meta.index_root).scan(&state.store).unwrap();
        let current = meta.current_data_page;
        let locs = locs.into_iter().map(|(_key, loc)| loc);
        let (owned, _chains) = data::collection_pages(&state.store, current, locs).unwrap();
        data::assert_free_space_is_true(&state.store, current, &owned, free);
    }
}

/// Only a document's id: any document reads as one, every other
/// field skipped (SPEC §99.4).
#[derive(Serialize, Deserialize)]
struct IdOnly {
    #[serde(rename = "_id")]
    id: DocId,
}

/// The keys of a document read through the typed path, values
/// skipped: what a typed read with a selection was given (SPEC §106.2).
#[derive(Deserialize)]
#[serde(transparent)]
struct Keys(std::collections::BTreeMap<String, serde::de::IgnoredAny>);

impl Serialize for Keys {
    fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
        unreachable!("only read")
    }
}

/// Paths to select from among `docs`' documents (SPEC §106): their
/// fields and the fields one level inside, its indexes' fields, `_id`,
/// a missing field, a step into an array, the document itself.
fn selectable(docs: &Collection<Document>, found: &[(DocId, Document)]) -> Vec<String> {
    let mut paths: Vec<String> = ["_id", "missing", "missing.x", "tags[*].x", ""]
        .map(String::from)
        .into();
    for (_id, doc) in found.iter().take(8) {
        let Document::Object(map) = doc else { continue };
        for (key, value) in map {
            paths.push(key.clone());
            if let Document::Object(inner) = value {
                paths.extend(inner.keys().map(|k| format!("{key}.{k}")));
            }
        }
    }
    for index in docs.indexes().unwrap() {
        paths.extend(index.fields().iter().cloned());
    }
    paths.sort();
    paths.dedup();
    paths
}

// --- Helpers the topic files share ---

// --- Helpers the topic files share ---
