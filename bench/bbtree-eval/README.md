# b-positive on bbtree's benchmark

[bbtree](https://github.com/XiangpengHao/bbtree) claims to be the fastest ordered map
([post](https://bbtree.xiangpeng.systems/post/)). Its evaluation harness,
[bbtree-eval](https://github.com/XiangpengHao/bbtree-eval), is public, so b-positive runs in it here, beside the maps
the post shows, on the post's single-thread table.

```sh
bench/bbtree-eval/run.sh                 # u64, uuid and str-json keys; results in target/bbtree-eval/results
```

- `run.sh` clones bbtree (6619c4e, the post's version; its API has changed since) and bbtree-eval (01229ec), applies
  `harness.patch` (registers the map; `ScalingKey::ordered_bytes`, a key's bytes in its order) and adds
  `b_positive.rs`, the adapter, and `sqlite.rs`, SQLite's (below).
- `results/`: the run below, one CSV per key set, and the machine; `results/first-run/`, the first (before #1-#3, on
  the same machine in a slower state: every map ran about 25% slower); `results/sqlite/`, the SQLite run.

## What is compared

Different kinds of map. bbtree and the others are in-memory maps: nodes on the heap, typed keys, changes in place.
b-positive is a persistent store: byte-string keys in 4 KB pages of one memory-mapped file, read in place, written
copy-on-write and committed so a crash leaves the last commit whole.

**The adapter:** a workload's writes are one write transaction, committed (durability `Process`: written to the OS,
not forced to disk) when the workload ends, inside its timed region. Reads go through one snapshot; batched lookups
through `ReadTxn::get_many`. Keys are their order-preserving bytes (big-endian integers, a UUID's bytes, a string's
UTF-8), values 8 little-endian bytes.

**The workloads** (bbtree-eval's): 10M entries built in random order; lookups of which half miss; inserts of every
key into an empty map; updates (add one to a value); scans of 100 entries from a random key; one thread, one second
per cell (inserts: all 10M). Batched: 64 lookups a call (bbtree's `get_many_copied`, b-positive's `get_many`), 16
inserts (bbtree's `insert_many`).

## Results

Apple M3 Max (36 GB), macOS 26.5, Rust 1.99, mimalloc; every map in one session, each cell once (cells vary about
±10% run to run here). Million operations per second, higher is better.

| Keys | Map | Lookups | Inserts | Updates | Scans |
|---|---|---:|---:|---:|---:|
| u64 | bbtree | 8.92 | 5.86 | 9.14 | 2.02 |
|  | bbtree, batched | 18.15 | 10.05 |  |  |
|  | std `BTreeMap` | 4.33 | 4.05 | 4.01 | 0.89 |
|  | **b-positive** | **3.00** | **2.18** | **1.25** | **0.84** |
|  | **b-positive, batched** | **5.51** |  |  |  |
|  | scc `TreeIndex` | 2.60 | 2.57 | 2.62 | 0.53 |
|  | concurrent-map | 3.23 | 2.12 | 1.42 | 1.34 |
|  | crossbeam `SkipMap` | 0.97 | 1.19 | 1.01 | 0.10 |
|  | std `HashMap` (no order) | 31.88 | 19.42 | 20.50 |  |
| uuid | bbtree | 7.92 | 5.01 | 7.98 | 1.60 |
|  | bbtree, batched | 15.21 | 8.84 |  |  |
|  | std `BTreeMap` | 2.91 | 3.07 | 2.83 | 0.55 |
|  | **b-positive** | **2.64** | **2.02** | **1.21** | **0.76** |
|  | **b-positive, batched** | **6.30** |  |  |  |
|  | scc `TreeIndex` | 2.01 | 2.05 | 1.99 | 0.47 |
|  | concurrent-map | 1.58 | 1.41 | 0.91 | 0.94 |
|  | crossbeam `SkipMap` | 0.82 | 0.98 | 0.82 | 0.10 |
|  | std `HashMap` (no order) | 22.60 | 16.80 | 17.71 |  |
| str-json | bbtree | 6.76 | 3.87 | 6.66 | 1.22 |
|  | bbtree, batched | 10.64 | 6.68 |  |  |
|  | std `BTreeMap` | 1.44 | 1.67 | 1.37 | 0.47 |
|  | **b-positive** | **2.52** | **1.91** | **1.10** | **0.83** |
|  | **b-positive, batched** | **4.83** |  |  |  |
|  | scc `TreeIndex` | 1.07 | 1.30 | 1.01 | 0.38 |
|  | concurrent-map | 0.77 | 0.40 | 0.27 | 0.24 |
|  | crossbeam `SkipMap` | 0.52 | 0.65 | 0.53 | 0.10 |
|  | std `HashMap` (no order) | 17.04 | 13.78 | 9.63 |  |

**Restart to first answer** (the post's bbtree-mmap card): b-positive opens a 10M-entry file (u64 keys, 230 MB) and
answers a lookup in 35–72 µs from the page cache. The post gives about 18 µs for bbtree-mmap, a read-only file written
once, on its own machine, and 45 ms for std's `BTreeMap` read back from a file.

- **Reads:** bbtree's lookups are 2.7–3.0× faster, batched or not, and its scans 1.5–2.4×. b-positive's lookups
  barely slow down on string keys (they sit flat in the page, not behind a pointer): on str-json they are 1.8× std's
  `BTreeMap`'s and 2.4× scc's; on uuid about level with std's and ahead of the concurrent maps; on u64 behind std's
  and concurrent-map. Batched, they beat std's `BTreeMap` on every key set (1.3–3.4×). Its scans beat std's, scc's
  and crossbeam's on every key set.
- **Writes:** inserts are 2.0–2.7× slower than bbtree's and updates 6–7×. Copy-on-write copies each page a
  transaction changes and the path above it, and a commit writes them, which a map in memory never does.

## Against SQLite

The maps above keep nothing on disk. SQLite does what b-positive does: one file, a B-tree in pages, transactions that
survive a crash. Its adapter (`sqlite.rs`, SQLite 3.46.0 bundled by rusqlite 0.32.1) keeps b-positive's rules:

- keys in their order-preserving bytes as the primary key of a `WITHOUT ROWID` table, so the B-tree holds the entries
  themselves; values as integers; statements prepared once and cached;
- a workload's writes in one transaction, committed when it ends, inside the timed region; reads in one read
  transaction;
- the WAL journal with `synchronous=OFF` (written to the OS, no fsync, as b-positive's `Process`); 4 KB pages; mmap
  and a 4 GB page cache, so it reads from memory as b-positive does;
- updates as one `UPDATE ... SET value = value + 1`; inserts as an upsert. SQLite has no batched lookup, so its batched
  cell is single lookups.

One session, the same harness and workloads; std's `BTreeMap` ran beside them and was 5–20% slower than in the table
above, so compare within this table. Million operations per second.

| Keys | Map | Lookups | Batched | Inserts | Updates | Scans |
|---|---|---:|---:|---:|---:|---:|
| u64 | **b-positive** | **2.76** | **5.57** | **1.93** | **1.18** | **0.83** |
|  | SQLite | 1.26 | 1.30 | 1.04 | 0.59 | 0.26 |
|  | std `BTreeMap` | 3.96 | 3.86 | 3.45 | 3.58 | 0.85 |
| uuid | **b-positive** | **2.69** | **6.02** | **1.88** | **1.15** | **0.71** |
|  | SQLite | 1.26 | 1.25 | 0.88 | 0.49 | 0.24 |
|  | std `BTreeMap` | 2.38 | 2.54 | 3.06 | 2.49 | 0.54 |
| str-json | **b-positive** | **2.41** | **5.05** | **1.74** | **1.10** | **0.78** |
|  | SQLite | 1.09 | 1.10 | 0.79 | 0.48 | 0.24 |
|  | std `BTreeMap` | 1.13 | 1.10 | 1.35 | 0.92 | 0.43 |

b-positive is 2.1–2.2× SQLite on lookups (4.3–4.8× batched), 1.9–2.2× on inserts, 2.0–2.3× on updates and 3.0–3.3×
on scans. Much of SQLite's cost per operation is outside its B-tree: binding parameters, its bytecode engine, and
copying each row out (b-positive reads values in place). SQLite is the more complete system (SQL, any schema,
secondary indexes, processes sharing one file); the comparison says what a key/value B-tree pays for going through it.

## What changed, and why bbtree is still faster

The first run had b-positive at 1.5–1.6M lookups/s, 0.42M inserts/s and 0.10M updates/s (bbtree 4–4.5× ahead on
lookups, 60× on updates). Three changes, each checked against every service's regression benchmark:

| | What | Effect |
|---|---|---|
| #2 | copied pages edited in place instead of decoded into vectors of owned keys; commits write pages in sorted runs | `examples/writes`: inserts 0.52 -> 1.28M/s, updates 0.22 -> 0.79M/s |
| #1 | slots carry a 2-byte key summary past the page's shared prefix; searches binary-search the slot array and compare full keys only on ties (file format 2) | lookups about +50%; `examples/writes`: inserts 1.28 -> 2.1M/s, updates 0.79 -> 1.04M/s |
| #3 | `ReadTxn::get_many`: a batch goes down the tree together, a level at a time, prefetching each next page | batched lookups 1.8–2.4× single ones |

It is not B-tree against B+tree: bbtree also keeps every value in a leaf (its separators are keys moved up, not
copies). What still differs:

| | bbtree | b-positive |
|---|---|---|
| Search in a node | 2–8-byte key summaries counted with vector compares | 2-byte summaries, binary-searched (a one-pass count over every slot measured slower here: it reads every slot's cache line) |
| Shared prefixes | per node, from its fences (deeper nodes skip more), and per map | per page, its keys' shared prefix |
| Key types | layouts specialized by type | byte strings (types belong to introvert's codecs) |
| Node memory | heap nodes sized for the cache | 4 KB pages read through mmap |
| Writes | in place, in memory | copy-on-write, committed to a file |
| Durability, snapshots, restart | none (bbtree-mmap: a read-only file written once) | crash-safe commits, snapshot readers, instant open |

None of the changes altered what b-positive is: commits stay copy-on-write, readers stay lock-free snapshots, keys stay
byte strings read in place. Left for later: truncating stored keys (smaller pages for long shared prefixes, at the
cost of cursors borrowing keys) and fence prefixes.
