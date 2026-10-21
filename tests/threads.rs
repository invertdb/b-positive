//! Readers on other threads, while one writer commits: every snapshot is internally consistent.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use b_positive::{Db, Options};

#[test]
fn readers_see_consistent_snapshots_while_the_writer_commits() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("threads.db"), Options::default()).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let db = db.clone();
            let done = done.clone();
            std::thread::spawn(move || {
                let mut checks = 0u64;
                while !done.load(Ordering::Relaxed) {
                    // every commit writes "version" = v and keys k0..k999 all = v
                    let r = db.begin_read();
                    let Some(version) = r.get(b"version").map(|v| v.into_owned()) else { continue };
                    for (k, v) in r.iter() {
                        if k != b"version" {
                            assert_eq!(&*v, version.as_slice(), "a snapshot mixes commits");
                        }
                    }
                    checks += 1;
                }
                checks
            })
        })
        .collect();
    for version in 0..300u32 {
        let mut w = db.begin_write();
        for i in 0..1000 {
            w.put(format!("k{i}").as_bytes(), &version.to_le_bytes()).unwrap();
        }
        w.put(b"version", &version.to_le_bytes()).unwrap();
        w.commit().unwrap();
    }
    done.store(true, Ordering::Relaxed);
    let checks: u64 = readers.into_iter().map(|t| t.join().unwrap()).sum();
    assert!(checks > 10, "readers ran ({checks} snapshots checked)");
}
