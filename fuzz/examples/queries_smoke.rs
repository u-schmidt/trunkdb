//! The `queries` target on random programs, on stable and without
//! libFuzzer (SPEC §105.4): `cargo run --release --example queries_smoke
//! -- <programs> [<seed>]`. A program that fails, or runs past a minute,
//! is written to `artifacts/queries/smoke-<seed>-<n>`, which the target
//! replays: `cargo +nightly fuzz run queries <file>`. At the end it prints
//! how often each kind of step ran and which plans came up. Build it with
//! `RUSTFLAGS="--cfg fuzzing"`, as `cargo fuzz` builds the targets: it
//! skips the flush of every commit (SPEC §55.1).
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use trunkdb_fuzz::queries::{Ran, STEP_NAMES, run};

/// The program running, since when, and its bytes: what the watchdog
/// keeps if it hangs.
type Current = Arc<Mutex<(usize, Instant, Vec<u8>)>>;

fn main() {
    if !cfg!(fuzzing) {
        eprintln!(
            "built without --cfg fuzzing: every commit is flushed, and a run is about 40× slower \
             (RUSTFLAGS=\"--cfg fuzzing\", as cargo fuzz sets it)"
        );
    }
    let mut args = std::env::args().skip(1);
    let programs: usize = args.next().map_or(1000, |n| n.parse().expect("a count"));
    let seed: u64 = args
        .next()
        .map_or(0x0105_F0CC_0105_F0CC, |s| s.parse().expect("a seed"));
    let mut rng = seed | 1;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    // A hang is a finding too: a program running past a minute is kept
    // and the run ends, as libFuzzer's timeout would end it.
    let current: Current = Arc::new(Mutex::new((0, Instant::now(), Vec::new())));
    let watched = Arc::clone(&current);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(5));
            let (n, since, input) = &*watched.lock().unwrap();
            if since.elapsed() > Duration::from_secs(60) {
                let file = keep(seed, *n, input);
                eprintln!("program {n} of seed {seed} hangs: {}", file.display());
                std::process::exit(2);
            }
        }
    });
    let mut total = Ran::default();
    let started = Instant::now();
    for n in 0..programs {
        // A program stops where its bytes do, and the prelude alone takes
        // up to about 6 KB: 4 to 20 KB leave room for the 64 steps.
        let len = 4096 + (next() % 16384) as usize;
        let input: Vec<u8> = (0..len).map(|_| next() as u8).collect();
        *current.lock().unwrap() = (n, Instant::now(), input.clone());
        match catch_unwind(AssertUnwindSafe(|| run(&input))) {
            Ok(ran) => {
                for (all, one) in total.steps.iter_mut().zip(ran.steps) {
                    *all += one;
                }
                total.plans.extend(ran.plans);
                total.answered += ran.answered;
                total.aggregated += ran.aggregated;
                total.unique_indexes += ran.unique_indexes;
                total.refused_duplicates += ran.refused_duplicates;
            }
            Err(_) => {
                let file = keep(seed, n, &input);
                eprintln!("program {n} of seed {seed} failed: {}", file.display());
                std::process::exit(1);
            }
        }
    }
    let took = started.elapsed();
    println!(
        "{programs} programs in {:.1} s, {:.0} a second",
        took.as_secs_f64(),
        programs as f64 / took.as_secs_f64()
    );
    for (name, n) in STEP_NAMES.iter().zip(total.steps) {
        println!("  {name:14} {n}");
    }
    println!("  answered from entries (§106.5): {}", total.answered);
    println!("  aggregated from entries (§107.5): {}", total.aggregated);
    println!("  unique indexes made (§119): {}", total.unique_indexes);
    println!(
        "  refused as duplicates (§119): {}",
        total.refused_duplicates
    );
    println!("plans:");
    for plan in &total.plans {
        println!("  {plan}");
    }
}

/// Writes a failing program where the target can replay it.
fn keep(seed: u64, n: usize, input: &[u8]) -> PathBuf {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("artifacts/queries");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!("smoke-{seed}-{n}"));
    std::fs::write(&file, input).unwrap();
    file
}
