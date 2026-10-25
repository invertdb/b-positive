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

- **Pages:** 4 KB pages in one file, memory-mapped for reading. Reads binary-search page bytes in
  place: no node cache, no per-key objects.
- **Nodes:** slotted pages. Variable-length keys (up to 1 KB) and values; values over 1 KB go to
  chains of overflow pages.
- **Copy-on-write:** a write transaction copies the pages it changes into new pages, and edits those copies in
  place (a change a page can't hold makes it a list of entries until it splits back into pages). A commit writes
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
| writes | 315k/s (341k/s with mimalloc) |
| random reads, 1 thread | 1.7–2.3M/s |
| random reads, 8 threads | 15.6–16.9M/s |
| file | 256 MB |

The library never sets an allocator: that's each binary's choice (`--features mimalloc` for the examples).

**Writes** (`cargo run --release --features mimalloc --example writes`): 10M u64 keys inserted in random order in one
transaction, 1.28M/s (its commit 0.2 s); 1M random updates of them in one transaction, 0.79M/s (commit 0.08 s).
Before copied pages were edited in place and commits wrote pages in runs (#2): 0.52M/s and 0.22M/s.

### Against in-memory ordered maps (`bench/bbtree-eval`)

bbtree's benchmark (10M entries, one thread, Apple M3 Max), million operations per second:

| u64 keys | Lookups | Inserts | Updates | Scans |
|---|---:|---:|---:|---:|
| bbtree | 6.78 | 3.72 | 5.93 | 1.63 |
| std `BTreeMap` | 3.72 | 2.85 | 3.46 | 0.80 |
| b-positive | 1.65 | 1.34 | 0.93 | 0.75 |
| scc `TreeIndex` | 1.92 | 1.96 | 2.02 | 0.44 |

On string keys b-positive's lookups (1.68M/s) beat std's `BTreeMap` (1.11M/s) and every concurrent map. Writes were
0.42M/s (inserts) and 0.10M/s (updates) before #2. What remains is searching within a page (#1) and batched lookups
(#3); details in `bench/bbtree-eval/README.md`.

## Not yet

- Rebalancing of underfull branches (leaves merge; an emptied branch is removed). Trees stay
  correct, but can become less dense after heavy deletes.
- Compaction (the file never shrinks).
- Typed keys and values here. Types live above this level, in introvert's order-preserving `Codec`.

## References

- Bayer, R., and McCreight, E. (1972). Organization and maintenance of large ordered indexes.
  *Acta Informatica* 1(3).
- Comer, D. (1979). The ubiquitous B-tree. *ACM Computing Surveys* 11(2).
- Graefe, G. (2011). Modern B-tree techniques. *Foundations and Trends in Databases* 3(4).
- Lorie, R. A. (1977). Physical integrity in a large segmented database. *ACM Transactions on Database Systems*
  2(1).
- Rodeh, O. (2008). B-trees, shadowing, and clones. *ACM Transactions on Storage* 3(4).
