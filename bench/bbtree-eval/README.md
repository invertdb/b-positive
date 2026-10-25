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
  `b_positive.rs`, the adapter.
- `results/`: the run below, one CSV per key set, and the machine.

## What is compared

Different kinds of map. bbtree and the others are in-memory maps: nodes on the heap, typed keys, changes in place.
b-positive is a persistent store: byte-string keys in 4 KB pages of one memory-mapped file, read in place, written
copy-on-write and committed so a crash leaves the last commit whole.

**The adapter:** a workload's writes are one write transaction, committed (durability `Process`: written to the OS,
not forced to disk) when the workload ends, inside its timed region. Reads go through one snapshot. Keys are their
order-preserving bytes (big-endian integers, a UUID's bytes, a string's UTF-8), values 8 little-endian bytes.

**The workloads** (bbtree-eval's): 10M entries built in random order; lookups of which half miss; inserts of every
key into an empty map; updates (add one to a value); scans of 100 entries from a random key; one thread, one second
per cell (inserts: all 10M). bbtree also through its batch calls (64 lookups, 16 inserts a call).

## Results

Apple M3 Max (36 GB), macOS 26.5, Rust 1.99, mimalloc; each cell run once, with the machine's swap nearly full.
Million operations per second, higher is better.

| Keys | Map | Lookups | Inserts | Updates | Scans |
|---|---|---:|---:|---:|---:|
| u64 | bbtree | 6.78 | 3.72 | 5.93 | 1.63 |
| | bbtree, batched | 12.29 | 7.51 | | |
| | std `BTreeMap` | 3.72 | 2.85 | 3.46 | 0.80 |
| | **b-positive** | **1.54** | **0.42** | **0.10** | **0.71** |
| | scc `TreeIndex` | 1.92 | 1.96 | 2.02 | 0.44 |
| | concurrent-map | 2.27 | 1.70 | 1.05 | 1.03 |
| | crossbeam `SkipMap` | 0.81 | 0.88 | 0.84 | 0.09 |
| | std `HashMap` (no order) | 18.46 | 15.30 | 16.66 | |
| uuid | bbtree | 7.08 | 3.72 | 6.44 | 1.34 |
| | bbtree, batched | 13.15 | 6.74 | | |
| | std `BTreeMap` | 2.30 | 2.47 | 2.37 | 0.51 |
| | **b-positive** | **1.51** | **0.42** | **0.10** | **0.67** |
| | scc `TreeIndex` | 1.71 | 1.64 | 1.73 | 0.45 |
| | concurrent-map | 1.44 | 1.11 | 0.85 | 0.78 |
| | crossbeam `SkipMap` | 0.63 | 0.74 | 0.68 | 0.09 |
| | std `HashMap` (no order) | 13.39 | 12.70 | 11.72 | |
| str-json | bbtree | 4.86 | 2.86 | 4.73 | 1.00 |
| | bbtree, batched | 8.98 | 5.03 | | |
| | std `BTreeMap` | 1.11 | 1.34 | 1.08 | 0.39 |
| | **b-positive** | **1.57** | **0.42** | **0.09** | **0.67** |
| | scc `TreeIndex` | 0.86 | 1.11 | 0.84 | 0.33 |
| | concurrent-map | 0.65 | 0.33 | 0.25 | 0.20 |
| | crossbeam `SkipMap` | 0.39 | 0.48 | 0.38 | 0.08 |
| | std `HashMap` (no order) | 14.12 | 11.97 | 8.37 | |

**Restart to first answer** (the post's bbtree-mmap card): b-positive opens a 10M-entry file (u64 keys, 230 MB) and
answers a lookup in 35–72 µs from the page cache. The post gives about 18 µs for bbtree-mmap, a read-only file written
once, on its own machine, and 45 ms for std's `BTreeMap` read back from a file.

- **Reads:** bbtree's lookups are 3–4.5× faster and its scans about 2×. b-positive's lookups don't slow down on
  string keys (they sit flat in the page, not behind a pointer), so there it beats std's `BTreeMap` and every
  concurrent map; on integers it is about level with scc and concurrent-map. Its scans beat std's, scc's and
  crossbeam's everywhere.
- **Writes:** inserts are 7–9× slower than bbtree's, updates about 60×. Copy-on-write copies each page it changes and
  the path above it, and commits write them out; and the writer decodes every page it copies into a vector of owned
  keys (b-positive#2).
- The adapter allocates each u64 key's bytes (a small cost on the u64 rows only). The fourth key set of the post,
  Wikipedia titles, needs bbtree-eval's `fetch` and isn't run.

## Why bbtree is faster, and what carries over

It is not B-tree against B+tree: bbtree also keeps every value in a leaf (its separators are keys moved up, not
copies). The differences:

| | bbtree | b-positive | Fix |
|---|---|---|---|
| Search in a node | a dense array of 2–8-byte key summaries, counted in one pass with vector compares; full keys only for ties | binary search of a slotted page: about 8 dependent probes, each to a key elsewhere in the page | key summaries (b-positive#1) |
| Shared prefixes | skipped, per node (from its fences) and per map | every key compared in full | in the summaries; keys stay whole, so the cursor still borrows them (b-positive#1) |
| Key types | layouts specialized by type | byte strings | none: types belong to introvert's codecs |
| Batches | interleaved searches with prefetching (1.8×) | one `get` at a time | `get_many` (b-positive#3) |
| Writes | in place, in memory | copy-on-write, committed to a file; copied pages decoded into owned keys | edit copied pages encoded, in place (b-positive#2) |
| Durability, snapshots, restart | none (bbtree-mmap: a read-only file written once) | crash-safe commits, snapshot readers, instant open | — |

None of the fixes changes what b-positive is: commits stay copy-on-write, readers stay lock-free snapshots, keys stay
byte strings read in place. #1 changes the page format. Writes will stay slower than an in-memory map's however fast
the page edits get: a commit copies and writes pages, which a map in memory never does.
