#[trunkdb::document]
#[derive(serde::Serialize, serde::Deserialize)]
struct Task {
    #[id(x)]
    id: Option<trunkdb::DocId>,
}

fn main() {}