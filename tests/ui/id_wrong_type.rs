#[trunkdb::document]
#[derive(serde::Serialize, serde::Deserialize)]
struct Task {
    #[id]
    id: String,
    title: String,
}

fn main() {}