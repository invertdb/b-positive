//! Microbenchmark: N random keys ("key:" + 10 digits) with 100-byte
//! values, committed every B puts; then N random gets on one thread, and on 8.
use std::time::Instant;

use b_positive::{Db, Options};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

/// With the `mimalloc` feature, a faster general-purpose allocator than the system's.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
    let batch: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(200);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bench.db");
    let db = Db::open(&path, Options::default()).unwrap();
    let mut rng = StdRng::seed_from_u64(1);
    let mut order: Vec<u32> = (0..n as u32).collect();
    for i in (1..order.len()).rev() {
        order.swap(i, rng.random_range(0..=i));
    }
    let key = |i: u32| format!("key:{i:010}");
    let mut value = vec![0u8; 100];

    let start = Instant::now();
    let mut w = db.begin_write();
    for (done, &i) in order.iter().enumerate() {
        rng.fill(&mut value[..]);
        w.put(key(i).as_bytes(), &value).unwrap();
        if (done + 1) % batch == 0 {
            w.commit().unwrap();
            w = db.begin_write();
        }
    }
    w.commit().unwrap();
    let seconds = start.elapsed().as_secs_f64();
    println!("set:  {n} keys in {seconds:.1} s, {:.0}/s (commit every {batch}); {} MB on disk", n as f64 / seconds, std::fs::metadata(&path).unwrap().len() >> 20);

    for _ in 0..2 {
        let start = Instant::now();
        let r = db.begin_read();
        let mut found = 0;
        for _ in 0..n {
            if r.get(key(rng.random_range(0..n as u32)).as_bytes()).is_some() {
                found += 1;
            }
        }
        let seconds = start.elapsed().as_secs_f64();
        println!("get:  {n} random in {seconds:.2} s, {:.0}/s, {found} found", n as f64 / seconds);
    }
    let start = Instant::now();
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let db = db.clone();
            std::thread::spawn(move || {
                let mut rng = StdRng::seed_from_u64(t);
                let r = db.begin_read();
                (0..n / 8).filter(|_| r.get(format!("key:{:010}", rng.random_range(0..n as u32)).as_bytes()).is_some()).count()
            })
        })
        .collect();
    let found: usize = threads.into_iter().map(|t| t.join().unwrap()).sum();
    let seconds = start.elapsed().as_secs_f64();
    println!("get:  {n} random on 8 threads in {seconds:.2} s, {:.0}/s, {found} found", n as f64 / seconds);
}
