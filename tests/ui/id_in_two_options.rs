#[trunkdb::document]
#[derive(serde::Serialize, serde::Deserialize)]
struct Task {
    #[id]
    id: Option<Option<trunkdb::DocId>>,
}

fn main() {}
