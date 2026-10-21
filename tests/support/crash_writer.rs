//! Writes batches forever (for tests/crash.rs to kill): batch n puts keys "b<n>/<i>" for i < 500 and sets "last" to n,
//! all in one commit, starting after whatever "last" the database already holds.
use b_positive::{Db, Options};

fn main() {
    let path = std::env::args().nth(1).expect("path");
    let db = Db::open(&path, Options::default()).unwrap();
    let mut n: u64 = db.begin_read().get(b"last").map(|v| u64::from_le_bytes(v[..8].try_into().unwrap()) + 1).unwrap_or(0);
    loop {
        let mut w = db.begin_write();
        for i in 0..500u32 {
            w.put(format!("b{n:08}/{i:04}").as_bytes(), &vec![(n % 251) as u8; (i as usize * 7) % 2000]).unwrap();
        }
        // and delete an older batch, so pages are freed and reused
        if n >= 3 {
            for i in 0..500u32 {
                w.delete(format!("b{:08}/{i:04}", n - 3).as_bytes());
            }
        }
        w.put(b"last", &n.to_le_bytes()).unwrap();
        w.commit().unwrap();
        n += 1;
    }
}
