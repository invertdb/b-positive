//! b-positive (invertdb's copy-on-write, memory-mapped B+tree of byte strings), as one thread owns it: keys in
//! their order-preserving bytes, values as 8 little-endian bytes. A handle's writes go in one write transaction,
//! committed (durability `Process`: written to the OS, no fsync) when the handle drops, inside the timed region;
//! its reads go through one snapshot of the last commit, read in place from the mapped file.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use b_positive::{Db, ReadTxn, WriteTxn};

use super::{MapHandle, ScalingKey, SharedMap, sum_of_first};

pub(crate) struct BPositive {
    db: &'static Db,
    path: PathBuf,
}

impl Drop for BPositive {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub(crate) enum State {
    Idle,
    Reading(ReadTxn),
    Writing(WriteTxn<'static>),
}

pub(crate) struct BPositiveHandle<'m> {
    map: &'m BPositive,
    state: RefCell<State>,
    /// The thread that opened the write transaction, which holds the writer's lock.
    writer: std::cell::Cell<Option<std::thread::ThreadId>>,
}

// SAFETY: the harness makes a handle on one thread and uses it on another, but a handle opens its write
// transaction (and so takes the writer's mutex) only when it is first used, and it commits that transaction, which
// releases the mutex, on the thread that opened it: `writer` checks it.
unsafe impl Send for BPositiveHandle<'_> {}

impl Drop for BPositiveHandle<'_> {
    fn drop(&mut self) {
        if let State::Writing(w) = std::mem::replace(self.state.get_mut(), State::Idle) {
            assert_eq!(self.writer.get(), Some(std::thread::current().id()), "committed on the thread that wrote");
            w.commit().expect("commit");
        }
    }
}

impl BPositiveHandle<'_> {
    fn writing<R>(&self, f: impl FnOnce(&mut WriteTxn<'static>) -> R) -> R {
        let mut state = self.state.borrow_mut();
        if !matches!(*state, State::Writing(_)) {
            *state = State::Writing(self.map.db.begin_write());
            self.writer.set(Some(std::thread::current().id()));
        }
        let State::Writing(w) = &mut *state else { unreachable!() };
        f(w)
    }

    fn reading<R>(&self, f: impl FnOnce(&ReadTxn) -> R) -> R {
        let mut state = self.state.borrow_mut();
        if !matches!(*state, State::Reading(_)) {
            if let State::Writing(w) = std::mem::replace(&mut *state, State::Idle) {
                assert_eq!(self.writer.get(), Some(std::thread::current().id()), "committed on the thread that wrote");
                w.commit().expect("commit");
            }
            *state = State::Reading(self.map.db.begin_read());
        }
        let State::Reading(r) = &*state else { unreachable!() };
        f(r)
    }
}

fn value_of(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().expect("8-byte values"))
}

impl<K: ScalingKey> SharedMap<K> for BPositive {
    const NAME: &'static str = "b-positive";
    const SHARED: bool = false;

    type Handle<'m> = BPositiveHandle<'m>;

    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::var_os("BPOSITIVE_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
        let path = directory.join(format!("b-positive-{}-{}.db", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_file(&path);
        let db = Box::leak(Box::new(Db::open(&path, Default::default()).expect("opening b-positive")));
        BPositive { db, path }
    }

    fn handle(&self) -> BPositiveHandle<'_> {
        BPositiveHandle { map: self, state: RefCell::new(State::Idle), writer: Default::default() }
    }
}

impl<K: ScalingKey> MapHandle<K> for BPositiveHandle<'_> {
    fn insert(&self, key: K, value: u64) {
        self.writing(|w| w.put(&key.ordered_bytes(), &value.to_le_bytes()).expect("put"));
    }

    fn get(&self, key: &K) -> Option<u64> {
        self.reading(|r| r.get(&key.ordered_bytes()).map(|v| value_of(&v)))
    }

    /// `ReadTxn::get_many`: the lookups go down the tree together, prefetching.
    fn get_batch<const N: usize>(&self, keys: [&K; N]) -> [Option<u64>; N] {
        let bytes = keys.map(|key| key.ordered_bytes());
        self.reading(|r| {
            let found = r.get_many(&bytes);
            std::array::from_fn(|i| found[i].as_ref().map(|v| value_of(v)))
        })
    }

    fn bump(&self, key: &K) {
        let key = key.ordered_bytes();
        self.writing(|w| {
            if let Some(v) = w.get(&key) {
                w.put(&key, &(value_of(&v) + 1).to_le_bytes()).expect("put");
            }
        });
    }

    fn remove(&self, key: &K) -> bool {
        self.writing(|w| w.delete(&key.ordered_bytes()))
    }

    fn scan(&self, start: &K, count: usize) -> (usize, u64) {
        self.reading(|r| sum_of_first(r.range_from(&start.ordered_bytes()).map(|(_, v)| value_of(&v)), count))
    }
}
