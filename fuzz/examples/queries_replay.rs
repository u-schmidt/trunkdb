//! Replays one `queries` program, on stable (SPEC §105.4): `QUERIES_TRACE=1
//! cargo run --release --example queries_replay -- <file>` says each step
//! before it runs, so a failure shows what led to it.
fn main() {
    let file = std::env::args().nth(1).expect("a program's file");
    let input = std::fs::read(&file).expect("a readable file");
    let ran = trunkdb_fuzz::queries::run(&input);
    println!("ran: {:?}", ran.steps);
}
