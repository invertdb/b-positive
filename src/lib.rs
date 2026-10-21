//! b-positive: a persistent, copy-on-write B+tree of byte-string keys and values, with snapshot readers.
//!
//! The file is a sequence of 4 KB pages, memory-mapped for reading. Pages 0 and 1 hold alternating meta pages;
//! a commit writes every changed page to a fresh location and then the meta page that points at the new root,
//! so the previous commit stays intact until the new one is complete. Readers see a snapshot and never block
//! or are blocked; one writer at a time.
//!
//! ```
//! let dir = tempfile::tempdir().unwrap();
//! let db = b_positive::Db::open(dir.path().join("data.db"), Default::default()).unwrap();
//! let mut w = db.begin_write();
//! w.put(b"hello", b"world").unwrap();
//! w.commit().unwrap();
//! assert_eq!(db.begin_read().get(b"hello").as_deref(), Some(&b"world"[..]));
//! ```

mod db;
pub mod page;

pub use db::{Cursor, Db, Durability, Options, ReadTxn, WriteTxn};
