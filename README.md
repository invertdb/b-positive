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

A B+tree (Bayer and McCreight, 1972; Comer, 1979) committed by shadow paging (Lorie, 1977), applied
copy-on-write to the whole tree (Rodeh, 2008).

- **Pages:** 4 KB pages in one file, memory-mapped for reading. Reads search page bytes in place: no node cache,
  no per-key objects.
- **Nodes:** slotted pages. Variable-length keys (up to 1 KB) and values; values over 1 KB go to chains of overflow
  pages. A page records the prefix its keys share (its first 8 bytes kept beside the header), and each slot a 2-byte
  summary of its key past that prefix, so a search binary-searches the slot array and compares full keys only where
  summaries tie (Bayer and Unterauer, 1977; Graefe, 2011). Keys are stored whole, so a cursor hands them out
  borrowed from the page.
- **Copy-on-write:** a write transaction copies the pages it changes into new pages, and edits those copies in
  place (a change a page can't hold makes it a list of entries until it splits back into pages). A commit writes
  them, then a meta page in the slot the previous commit didn't use. There are two meta pages, each
  with a transaction id and CRC32, and opening takes the newest valid one. A crash at any point
  therefore leaves the previous commit intact.
- **Durability:** `Durability::Process` (the default) writes to the operating system, which survives
  the process dying. `Durability::Full` forces data, then the meta page, to stable storage, which
  survives power loss.
- **Readers:** snapshots (`ReadTxn`). Any number, on any threads, never blocked by or blocking the
  one writer. `get_many` looks up a batch of keys together, prefetching, so their memory waits overlap.
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
| writes | 349k/s (mimalloc) |
| random reads, 1 thread | 2.1–2.4M/s |
| random reads, 8 threads | 16.6M/s |
| file | 256 MB |

The library never sets an allocator: that's each binary's choice (`--features mimalloc` for the examples).

**Writes** (`cargo run --release --features mimalloc --example writes`): 10M u64 keys inserted in random order in one
transaction, 2.1M/s (its commit 0.14 s); 1M random updates of them in one transaction, 1.04M/s (commit 0.06 s).
Before copied pages were edited in place and commits wrote pages in runs (#2): 0.52M/s and 0.22M/s; before key
summaries (#1): 1.28M/s and 0.79M/s.

### Against in-memory ordered maps (`bench/bbtree-eval`)

bbtree's benchmark (10M entries, one thread, Apple M3 Max, every map in one session), million operations per second:

| u64 keys | Lookups | Batched lookups | Inserts | Updates | Scans |
|---|---:|---:|---:|---:|---:|
| bbtree | 8.92 | 18.15 | 5.86 | 9.14 | 2.02 |
| std `BTreeMap` | 4.33 | 4.25 | 4.05 | 4.01 | 0.89 |
| b-positive | 3.00 | 5.51 | 2.18 | 1.25 | 0.84 |
| scc `TreeIndex` | 2.60 | | 2.57 | 2.62 | 0.53 |

On string keys b-positive's lookups (2.52M/s) are 1.8× std's `BTreeMap`'s and ahead of every concurrent map. Batched
lookups (`ReadTxn::get_many`, #3) run 1.8–2.4× single ones. The first run, before #1-#3, had b-positive at 1.54 / -
/ 0.42 / 0.10 / 0.71. Details in `bench/bbtree-eval/README.md`.

Against SQLite in the same harness and under the same rules (one file, a `WITHOUT ROWID` table, WAL without fsync,
one transaction per workload, mmap and a large page cache), b-positive is 2.1–2.2× faster on lookups (4.3–4.8×
batched), 1.9–2.2× on inserts, 2.0–2.3× on updates and 3.0–3.3× on scans.

## Not yet

- Rebalancing of underfull branches (leaves merge; an emptied branch is removed). Trees stay
  correct, but can become less dense after heavy deletes.
- Compaction in place: the file never shrinks while open. `Db::compact_to` writes a compact copy (entries appended
  in order, pages full, no free pages) for the caller to swap in.
- Typed keys and values here. Types live above this level, in introvert's order-preserving `Codec`.

## References

- Bayer, R., and McCreight, E. (1972). Organization and maintenance of large ordered indexes.
  *Acta Informatica* 1(3).
- Comer, D. (1979). The ubiquitous B-tree. *ACM Computing Surveys* 11(2).
- Bayer, R., and Unterauer, K. (1977). Prefix B-trees. *ACM Transactions on Database Systems* 2(1).
- Graefe, G. (2011). Modern B-tree techniques. *Foundations and Trends in Databases* 3(4).
- Lorie, R. A. (1977). Physical integrity in a large segmented database. *ACM Transactions on Database Systems*
  2(1).
- Rodeh, O. (2008). B-trees, shadowing, and clones. *ACM Transactions on Storage* 3(4).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.
Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate by you, as
defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
