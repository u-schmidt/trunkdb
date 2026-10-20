#[trunkdb::document]
#[derive(serde::Serialize, serde::Deserialize)]
struct Task {
    #[id]
    id: Option<trunkdb::DocId>,
    #[id]
    other: Option<trunkdb:DocId>,
}

fn main() {}