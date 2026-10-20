#![cfg(feature = "derive")]

use serde::{Deserialize, Serialize};
use trunkdb::{Database, DocId};

#[trunkdb::document]
#[derive(Serialize, Deserialize)]
struct Task {
    #[id]
    id: Option<DocId>,
    title: String,
}

#[test]
fn id_marks_the_documents_id() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("t.db")).unwrap();
    let tasks = db.collection::<Task>("tasks");
    let id = tasks
        .insert(Task {
            id: None,
            title: "learn macros".into(),
        })
        .unwrap();
    let task = tasks.get(&id).unwrap().unwrap();
    assert_eq!(task.id, Some(id));
    assert_eq!(task.title, "learn macros");
}

#[test]
fn mistakes_are_refused_at_compile_time() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/*.rs");
}

#[trunkdb::document]
#[derive(Serialize, Deserialize)]
struct Note {
    #[id]
    id: DocId,
    text: String,
}

#[test]
fn id_can_be_a_plain_doc_id() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("t.db")).unwrap();
    let notes = db.collection::<Note>("notes");
    let id: DocId = "0192f0c1-0000-7000-8000-000000000001".parse().unwrap();
    notes
        .insert(Note {
            id,
            text: "given by the caller".into(),
        })
        .unwrap();
    let note = notes.get(&id).unwrap().unwrap();
    assert_eq!(note.id, id);
    assert_eq!(note.text, "given by the caller");
}

#[trunkdb::document]
#[derive(Serialize, Deserialize)]
struct Event {
    name: String,
    at: i64,
    #[id]
    id: Option<trunkdb::DocId>,
}

#[test]
fn id_can_come_after_other_fields() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("t.db")).unwrap();
    let events = db.collection::<Event>("events");
    let id = events
        .insert(Event {
            name: "started".into(),
            at: 1,
            id: None,
        })
        .unwrap();
    let event = events.get(&id).unwrap().unwrap();
    assert_eq!(event.id, Some(id));
    assert_eq!((event.name.as_str(), event.at), ("started", 1));
}
