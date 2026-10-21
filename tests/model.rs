//! The database against an in-memory model (BTreeMap): random writes, deletes, commits, aborts and reopens,
//! with old snapshots held open across later commits.

use std::collections::BTreeMap;

use b_positive::{Db, Durability, Options, ReadTxn};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

fn key(rng: &mut StdRng) -> Vec<u8> {
    let n: u32 = rng.random_range(0..3000);
    let mut key = format!("key{n:05}").into_bytes();
    // some long keys, up to the limit
    if n % 97 == 0 {
        key.extend(std::iter::repeat_n(b'x', rng.random_range(0..1000)));
    }
    key
}

fn value(rng: &mut StdRng) -> Vec<u8> {
    let len = match rng.random_range(0..20) {
        0 => rng.random_range(1025..9000), // overflow pages
        1 => 0,
        _ => rng.random_range(1..200),
    };
    (0..len).map(|_| rng.random()).collect()
}

fn check(snapshot: &ReadTxn, model: &BTreeMap<Vec<u8>, Vec<u8>>, context: &str) {
    assert_eq!(snapshot.len(), model.len() as u64, "{context}: count");
    let all: Vec<(Vec<u8>, Vec<u8>)> = snapshot.iter().map(|(k, v)| (k.to_vec(), v.into_owned())).collect();
    assert_eq!(all.len(), model.len(), "{context}: iterated count");
    for ((k, v), (mk, mv)) in all.iter().zip(model.iter()) {
        assert_eq!(k, mk, "{context}: key order");
        assert_eq!(v, mv, "{context}: value of {}", String::from_utf8_lossy(&k[..k.len().min(20)]));
    }
    for (k, v) in model.iter().step_by(37) {
        assert_eq!(snapshot.get(k).as_deref(), Some(v.as_slice()), "{context}: get");
    }
    assert_eq!(snapshot.get(b"absent"), None);
}

#[test]
fn matches_a_model_through_commits_aborts_reopens_and_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("model.db");
    let seed = std::env::var("B_POSITIVE_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(42);
    let mut rng = StdRng::seed_from_u64(seed);
    let mut db = Db::open(&path, Options::default()).unwrap();
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut snapshots: Vec<(ReadTxn, BTreeMap<Vec<u8>, Vec<u8>>)> = Vec::new();

    for round in 0..300 {
        let mut working = model.clone();
        let mut w = db.begin_write();
        for _ in 0..rng.random_range(1..200) {
            let k = key(&mut rng);
            if rng.random_range(0..4) == 0 {
                assert_eq!(w.delete(&k), working.remove(&k).is_some(), "round {round}: delete result");
            } else {
                let v = value(&mut rng);
                w.put(&k, &v).unwrap();
                working.insert(k, v);
            }
        }
        assert_eq!(w.len(), working.len() as u64);
        if rng.random_range(0..6) == 0 {
            drop(w); // abort
        } else {
            w.commit().unwrap();
            model = working;
        }

        check(&db.begin_read(), &model, &format!("round {round}"));

        // hold some snapshots across later commits; they must keep seeing their own state
        if rng.random_range(0..5) == 0 {
            snapshots.push((db.begin_read(), model.clone()));
        }
        if snapshots.len() > 4 || (rng.random_range(0..4) == 0 && !snapshots.is_empty()) {
            let (snapshot, expected) = snapshots.remove(0);
            check(&snapshot, &expected, &format!("round {round}: snapshot of txn {}", snapshot.txn()));
        }

        if round % 50 == 49 {
            snapshots.clear();
            drop(db);
            db = Db::open(&path, Options::default()).unwrap();
            check(&db.begin_read(), &model, &format!("round {round}: reopened"));
        }
    }

    // deleting everything leaves an empty tree, and the space is reused rather than grown
    let size_before = std::fs::metadata(&path).unwrap().len();
    let mut w = db.begin_write();
    for k in model.keys() {
        assert!(w.delete(k));
    }
    w.commit().unwrap();
    assert!(db.begin_read().is_empty());
    let mut w = db.begin_write();
    for (k, v) in &model {
        w.put(k, v).unwrap();
    }
    w.commit().unwrap();
    check(&db.begin_read(), &model, "refilled");
    assert!(std::fs::metadata(&path).unwrap().len() <= size_before * 2, "free pages are reused");
}

#[test]
fn many_keys_and_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("big.db"), Options { durability: Durability::Process }).unwrap();
    let mut w = db.begin_write();
    for i in 0..200_000u32 {
        w.put(format!("{i:08}").as_bytes(), &i.to_le_bytes()).unwrap();
    }
    w.commit().unwrap();
    let r = db.begin_read();
    assert_eq!(r.len(), 200_000);
    assert_eq!(r.get(b"00123456").as_deref(), Some(&123456u32.to_le_bytes()[..]));
    let from: Vec<u32> = r.range_from(b"00199990").map(|(_, v)| u32::from_le_bytes(v[..4].try_into().unwrap())).collect();
    assert_eq!(from, (199_990..200_000).collect::<Vec<_>>());
    assert_eq!(r.range_from(b"0010000").next().map(|(k, _)| k.to_vec()), Some(b"00100000".to_vec()));
    assert_eq!(r.iter().count(), 200_000);
}

#[test]
fn many_small_commits_keep_the_file_and_free_list_small() {
    // a server commits a handful of writes at a time: tens of thousands of commits must neither grow the file nor
    // slow commits down (each commit writes the free list, so it must stay as small as the free space)
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("small.db");
    let db = Db::open(&path, Options::default()).unwrap();
    let started = std::time::Instant::now();
    let mut slowest = std::time::Duration::ZERO;
    for i in 0..20_000u32 {
        let begun = std::time::Instant::now();
        let mut w = db.begin_write();
        w.put(format!("key{:04}", (i * 7919) % 1000).as_bytes(), &[i as u8; 100]).unwrap();
        w.commit().unwrap();
        slowest = slowest.max(begun.elapsed());
    }
    let size = std::fs::metadata(&path).unwrap().len();
    assert_eq!(db.begin_read().len(), 1000);
    // 1,000 keys of ~110 bytes fit in about 40 pages; a few hundred pages of churn and headroom is fine
    assert!(size <= 4 << 20, "file grew to {size} bytes");
    assert!(slowest < std::time::Duration::from_millis(50), "slowest commit {slowest:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(20), "20,000 commits took {:?}", started.elapsed());
    drop(db);
    let db = Db::open(&path, Options::default()).unwrap();
    assert_eq!(db.begin_read().len(), 1000, "reopens with its free list");
}
