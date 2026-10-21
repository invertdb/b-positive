//! Kill a writer at random moments (SIGKILL, mid-commit included); the database must open at exactly some
//! complete commit: batch "last" fully present, the two before it present, nothing newer.
use std::process::Command;
use std::time::Duration;

use b_positive::{Db, Options};

#[test]
fn survives_being_killed_mid_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crash.db");
    let writer = env!("CARGO_BIN_EXE_crash_writer_helper");
    let mut last_seen = None;
    for round in 0..12u64 {
        let mut child = Command::new(writer).arg(&path).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(150 + (round * 37) % 300));
        child.kill().unwrap();
        child.wait().unwrap();

        let db = Db::open(&path, Options::default()).unwrap();
        let r = db.begin_read();
        let Some(last) = r.get(b"last").map(|v| u64::from_le_bytes(v[..8].try_into().unwrap())) else { continue };
        for n in last.saturating_sub(2)..=last {
            for i in 0..500u32 {
                let value = r.get(format!("b{n:08}/{i:04}").as_bytes()).unwrap_or_else(|| panic!("round {round}: batch {n} key {i} missing (last {last})"));
                assert_eq!(value.len(), (i as usize * 7) % 2000);
                assert!(value.iter().all(|&b| b == (n % 251) as u8));
            }
        }
        assert!(r.range_from(format!("b{:08}/", last + 1).as_bytes()).next().filter(|(k, _)| k.starts_with(b"b")).is_none(), "round {round}: data past the last commit");
        assert_eq!(r.len(), 3 * 500 + 1, "round {round}: exactly three batches and the marker");
        assert!(last_seen.is_none_or(|seen| last >= seen), "round {round}: went backwards");
        last_seen = Some(last);
    }
    assert!(last_seen.unwrap_or(0) > 0, "the writer made progress");
}
