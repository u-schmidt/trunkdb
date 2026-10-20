#[trunkdb::document]
#[derive(serde::Serialize, serde::Deserialize)]
struct Task {
    #[id]
    id: Vec<trunkdb::DocId>,
}

fn main() {}
