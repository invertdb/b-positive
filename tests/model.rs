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

/// How many bytes of the old value's length a sorted batch's value records: 0 to 2, so values both stay and change size.
fn rng_free_tag(i: usize) -> usize {
    i % 3
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
        if rng.random_range(0..3) == 0 {
            // a sorted batch, each new value made from the old one (so the old one passed in is checked too)
            let keys: Vec<Vec<u8>> = (0..rng.random_range(1..400)).map(|_| key(&mut rng)).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
            let fresh: Vec<Vec<u8>> = keys.iter().map(|_| value(&mut rng)).collect();
            let expected: Vec<Option<Vec<u8>>> = keys.iter().map(|k| working.get(k).cloned()).collect();
            w.put_sorted(&keys, |i, old| {
                assert_eq!(old, expected[i].as_deref(), "round {round}: old value");
                let mut v = fresh[i].clone();
                v.extend_from_slice(&old.map_or(0, <[u8]>::len).to_le_bytes()[..rng_free_tag(i)]);
                v
            })
            .unwrap();
            for (i, k) in keys.into_iter().enumerate() {
                let mut v = fresh[i].clone();
                v.extend_from_slice(&expected[i].as_ref().map_or(0, Vec::len).to_le_bytes()[..rng_free_tag(i)]);
                working.insert(k, v);
            }
        }
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
fn sorted_inserts_fill_their_pages() {
    // a sorted bulk load (every insert at the right edge) should leave pages nearly full, not half full
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sorted.db");
    let db = Db::open(&path, Options { durability: Durability::Process }).unwrap();
    let count = 200_000u32;
    let mut w = db.begin_write();
    for i in 0..count {
        w.put(format!("{i:016}").as_bytes(), &i.to_le_bytes()).unwrap();
    }
    w.commit().unwrap();
    assert_eq!(db.begin_read().len(), count as u64);
    // 16-byte key, 4-byte value: 6 bytes of entry header and a slot each
    let entry = 16 + 4 + 6 + b_positive::page::SLOT;
    let ideal = count as u64 * entry as u64;
    let size = std::fs::metadata(&path).unwrap().len();
    assert!(size < ideal * 5 / 4, "{size} bytes for {ideal} bytes of entries: pages aren't filling");

    // the same entries in random order take appreciably more room (balanced splits leave pages ~70% full)
    let shuffled = dir.path().join("shuffled.db");
    let db = Db::open(&shuffled, Options { durability: Durability::Process }).unwrap();
    let mut order: Vec<u32> = (0..count).collect();
    let mut state = 1u64;
    for i in (1..order.len()).rev() {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        order.swap(i, (state >> 33) as usize % (i + 1));
    }
    let mut w = db.begin_write();
    for &i in &order {
        w.put(format!("{i:016}").as_bytes(), &i.to_le_bytes()).unwrap();
    }
    w.commit().unwrap();
    assert_eq!(db.begin_read().len(), count as u64);
    let random_size = std::fs::metadata(&shuffled).unwrap().len();
    assert!(size < random_size, "sorted {size} bytes vs random {random_size}");
}

#[test]
fn sorted_batches_fill_their_pages() {
    // put_sorted over a bulk load in batches, then a batch merged into the middle: pages full, every key there
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("batches.db");
    let db = Db::open(&path, Options { durability: Durability::Process }).unwrap();
    let count = 200_000u32;
    let keys: Vec<Vec<u8>> = (0..count).map(|i| format!("{:016}", i * 2).into_bytes()).collect();
    for batch in keys.chunks(30_000) {
        let mut w = db.begin_write();
        w.put_sorted(batch, |i, old| {
            assert_eq!(old, None);
            (i as u32).to_le_bytes().to_vec()
        })
        .unwrap();
        w.commit().unwrap();
    }
    let ideal = count as u64 * (16 + 4 + 6 + b_positive::page::SLOT as u64);
    let size = std::fs::metadata(&path).unwrap().len();
    assert!(size < ideal * 5 / 4, "{size} bytes for {ideal} bytes of entries: pages aren't filling");

    // odd keys in between, and every tenth even key again: each lands in its leaf, which splits as it must
    let mut middle: Vec<Vec<u8>> = (0..count).filter(|i| i % 10 == 0).flat_map(|i| [format!("{:016}", i * 2), format!("{:016}", i * 2 + 1)]).map(String::into_bytes).collect();
    middle.sort();
    let mut w = db.begin_write();
    w.put_sorted(&middle, |_, old| old.map_or(b"new".to_vec(), |o| [o, b"+"].concat())).unwrap();
    w.commit().unwrap();
    let r = db.begin_read();
    assert_eq!(r.len(), count as u64 + count as u64 / 10);
    let mut previous: Option<Vec<u8>> = None;
    for (k, v) in r.iter() {
        assert!(previous.as_deref().is_none_or(|p| p < k), "order");
        let n: u32 = std::str::from_utf8(k).unwrap().parse().unwrap();
        match (n % 2, (n / 2) % 10) {
            (1, _) => assert_eq!(&*v, b"new"),
            (0, 0) => assert_eq!(v.last(), Some(&b'+')),
            _ => assert_eq!(v.len(), 4),
        }
        previous = Some(k.to_vec());
    }
}

#[test]
fn a_sorted_batch_into_an_empty_tree_splits_the_root_many_ways() {
    // one batch of large values: the first leaf splits into hundreds of pages, its new root into several
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("wide.db"), Options::default()).unwrap();
    let keys: Vec<Vec<u8>> = (0..60_000u32).map(|i| format!("{i:08}{}", "k".repeat(200)).into_bytes()).collect();
    let mut w = db.begin_write();
    w.put_sorted(&keys, |i, _| vec![i as u8; 300]).unwrap();
    w.commit().unwrap();
    let r = db.begin_read();
    assert_eq!(r.len(), keys.len() as u64);
    for (i, (k, v)) in r.iter().enumerate() {
        assert_eq!(k, keys[i].as_slice());
        assert_eq!(&*v, &vec![i as u8; 300][..]);
    }
    assert_eq!(r.get(&keys[31_337]).as_deref(), Some(&vec![(31_337 % 256) as u8; 300][..]));
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
