//! SQLite (bundled by rusqlite), held to b-positive's rules: one file, keys in their order-preserving bytes as the
//! primary key of a `WITHOUT ROWID` table (a B-tree of the entries themselves), values as integers. Each handle is its
//! own connection; its writes go in one transaction, committed when the handle drops, inside the timed region; its
//! reads go through one read transaction (a snapshot). The journal is WAL with `synchronous=OFF` (written to the OS,
//! no fsync: b-positive's `Process` durability); reads go through mmap and a page cache large enough for the table,
//! as b-positive's go through the OS page cache. Statements are prepared once and cached.

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{Connection, params};

use super::{MapHandle, ScalingKey, SharedMap, sum_of_first};

pub(crate) struct Sqlite {
    path: PathBuf,
}

impl Drop for Sqlite {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Idle,
    Reading,
    Writing,
}

pub(crate) struct SqliteHandle {
    connection: Connection,
    state: Cell<State>,
}

impl Drop for SqliteHandle {
    fn drop(&mut self) {
        if self.state.get() != State::Idle {
            self.connection.execute_batch("COMMIT").expect("commit");
        }
    }
}

impl SqliteHandle {
    /// Ends the open transaction unless it is `want`, and begins one of that kind.
    fn enter(&self, want: State) {
        let state = self.state.get();
        if state == want {
            return;
        }
        if state != State::Idle {
            self.connection.execute_batch("COMMIT").expect("commit");
        }
        self.connection.execute_batch(if want == State::Writing { "BEGIN IMMEDIATE" } else { "BEGIN" }).expect("begin");
        self.state.set(want);
    }
}

impl<K: ScalingKey> SharedMap<K> for Sqlite {
    const NAME: &'static str = "sqlite";
    const SHARED: bool = false;

    type Handle<'m> = SqliteHandle;

    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::var_os("BPOSITIVE_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
        let path = directory.join(format!("sqlite-{}-{}.db", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let map = Sqlite { path };
        drop(map.connect().execute_batch("CREATE TABLE map (key BLOB PRIMARY KEY, value INTEGER NOT NULL) WITHOUT ROWID"));
        map
    }

    fn handle(&self) -> SqliteHandle {
        SqliteHandle { connection: self.connect(), state: Cell::new(State::Idle) }
    }
}

impl Sqlite {
    fn connect(&self) -> Connection {
        let connection = Connection::open(&self.path).expect("opening sqlite");
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = OFF;
                 PRAGMA cache_size = -4194304;
                 PRAGMA mmap_size = 2147418112;",
            )
            .expect("pragmas");
        connection.set_prepared_statement_cache_capacity(16);
        connection
    }
}

impl<K: ScalingKey> MapHandle<K> for SqliteHandle {
    fn insert(&self, key: K, value: u64) {
        self.enter(State::Writing);
        let mut statement = self
            .connection
            .prepare_cached("INSERT INTO map VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value")
            .expect("prepare");
        statement.execute(params![&*key.ordered_bytes(), value as i64]).expect("insert");
    }

    fn get(&self, key: &K) -> Option<u64> {
        self.enter(State::Reading);
        let mut statement = self.connection.prepare_cached("SELECT value FROM map WHERE key = ?1").expect("prepare");
        let mut rows = statement.query(params![&*key.ordered_bytes()]).expect("select");
        rows.next().expect("step").map(|row| row.get::<_, i64>(0).expect("value") as u64)
    }

    fn bump(&self, key: &K) {
        self.enter(State::Writing);
        let mut statement =
            self.connection.prepare_cached("UPDATE map SET value = value + 1 WHERE key = ?1").expect("prepare");
        statement.execute(params![&*key.ordered_bytes()]).expect("update");
    }

    fn remove(&self, key: &K) -> bool {
        self.enter(State::Writing);
        let mut statement = self.connection.prepare_cached("DELETE FROM map WHERE key = ?1").expect("prepare");
        statement.execute(params![&*key.ordered_bytes()]).expect("delete") > 0
    }

    fn scan(&self, start: &K, count: usize) -> (usize, u64) {
        self.enter(State::Reading);
        let mut statement = self
            .connection
            .prepare_cached("SELECT value FROM map WHERE key >= ?1 ORDER BY key LIMIT ?2")
            .expect("prepare");
        let values = statement
            .query_map(params![&*start.ordered_bytes(), count as i64], |row| row.get::<_, i64>(0))
            .expect("select")
            .map(|value| value.expect("value") as u64);
        sum_of_first(values, count)
    }
}
