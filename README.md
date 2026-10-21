# b-positive

A persistent, copy-on-write B+tree of byte-string keys and values, with snapshot readers. The storage core of
invertdb.

```rust
let db = b_positive::Db::open("data.db", Default::default())?;
let mut w = db.begin_write();
w.put(b"hello", b"world")?;
w.commit()?;
let r = db.begin_read();                       // a snapshot: lock-free, any thread
assert_eq!(r.get(b"hello").as_deref(), Some(&b"world"[..]));
for (key, value) in r.range_from(b"h") { /* in key order */ }
```

## Design

- **Pages:** 4 KB pages in one file, memory-mapped for reading. Reads binary-search page bytes in
  place: no node cache, no per-key objects.
- **Nodes:** slotted pages. Variable-length keys (up to 1 KB) and values; values over 1 KB go to
  chains of overflow pages.
- **Copy-on-write:** a write transaction copies the pages it changes into new pages. A commit writes
  them, then a meta page in the slot the previous commit didn't use. There are two meta pages, each
  with a transaction id and CRC32, and opening takes the newest valid one. A crash at any point
  therefore leaves the previous commit intact.
- **Durability:** `Durability::Process` (the default) writes to the operating system, which survives
  the process dying. `Durability::Full` forces data, then the meta page, to stable storage, which
  survives power loss.
- **Readers:** snapshots (`ReadTxn`). Any number, on any threads, never blocked by or blocking the
  one writer.
- **Free pages:** reused only once no open snapshot can still see them. The free list is persisted
  as a page chain written fresh by each commit.

## Tests

- `tests/model.rs`: random puts, deletes, commits, aborts and reopens against a `BTreeMap`, with old
  snapshots held across later commits (any seed: `B_POSITIVE_SEED=n`; 30 seeds checked).
- `tests/crash.rs`: a writer killed with SIGKILL at random moments; the database must reopen at
  exactly one complete commit.
- `tests/threads.rs`: readers on four threads never see a mixed snapshot while the writer commits.

## Performance (`cargo run --release --example bench`)

1M random 14-byte keys with 100-byte values, committed every 200, on Apple Silicon:

| | b-positive |
|---|---:|
| writes | 196k/s |
| random reads, 1 thread | 2.29M/s |
| random reads, 8 threads | 16.9M/s |
| file | 256 MB |

## Not yet

- Rebalancing of underfull branches (leaves merge; an emptied branch is removed). Trees stay
  correct, but can become less dense after heavy deletes.
- Compaction (the file never shrinks).
- Typed keys and values here. Types live above this level, in introvert's order-preserving `Codec`.
