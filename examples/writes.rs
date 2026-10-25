//! The write path, as bbtree-eval exercises it (`bench/bbtree-eval`), split into the puts and the commit:
//! N u64 keys (big-endian) with 8-byte values inserted in random order in one transaction; then N/10 random
//! updates (get, then put) of the built tree in one transaction.
//!
//! ```text
//! cargo run --release --features mimalloc --example writes [N]     # default 10M
//! ```
use std::time::Instant;

use b_positive::{Db, Options};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

#[cfg(feature = "mimalloc")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    let n: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(10_000_000);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writes.db");
    let db = Db::open(&path, Options::default()).unwrap();
    let mut rng = StdRng::seed_from_u64(1);
    let mut order: Vec<u64> = (0..n).collect();
    for i in (1..order.len()).rev() {
        order.swap(i, rng.random_range(0..=i));
    }

    let start = Instant::now();
    let mut w = db.begin_write();
    for &k in &order {
        w.put(&k.to_be_bytes(), &k.to_le_bytes()).unwrap();
    }
    let puts = start.elapsed().as_secs_f64();
    w.commit().unwrap();
    let total = start.elapsed().as_secs_f64();
    println!(
        r#"{{"phase":"insert","operations":{n},"puts_seconds":{puts:.2},"commit_seconds":{:.2},"mops":{:.3},"file_mb":{}}}"#,
        total - puts,
        n as f64 / total / 1e6,
        std::fs::metadata(&path).unwrap().len() >> 20
    );

    let updates = n / 10;
    let start = Instant::now();
    let mut w = db.begin_write();
    for _ in 0..updates {
        let key = rng.random_range(0..n).to_be_bytes();
        let value = w.get(&key).unwrap();
        let next = u64::from_le_bytes(value[..].try_into().unwrap()) + 1;
        w.put(&key, &next.to_le_bytes()).unwrap();
    }
    let puts = start.elapsed().as_secs_f64();
    w.commit().unwrap();
    let total = start.elapsed().as_secs_f64();
    println!(
        r#"{{"phase":"update","operations":{updates},"puts_seconds":{puts:.2},"commit_seconds":{:.2},"mops":{:.3}}}"#,
        total - puts,
        updates as f64 / total / 1e6
    );
}
