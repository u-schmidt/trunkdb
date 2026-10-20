#[trunkdb::document(x)]
#[derive(serde::Serialize, serde::Deserialize)]
struct Task {
  #[id]
  id: Option<document::DocId>,
}

fn main() {}