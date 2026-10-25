#!/usr/bin/env bash
# bbtree-eval's single-thread table (the bbtree post's "Single thread" card, its blog/runs.sh tab1) with b-positive
# beside the other maps. Clones bbtree and bbtree-eval at the commits the post was run with into WORK, adds
# b-positive's adapter, and runs every cell: 10M entries, one thread.
#
#   bench/bbtree-eval/run.sh [keys...]        # default: u64 uuid str-json (wiki-titles needs bbtree-eval's `fetch`)
#
# Environment: WORK (default target/bbtree-eval), ENTRIES (10000000), GUARD (a wrapper such as the time/memory
# guard, prefixed to each run; default none).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
b_positive=$(cd "$here/../.." && pwd)
work=${WORK:-$b_positive/target/bbtree-eval}
entries=${ENTRIES:-10000000}
keys=("$@")
[ ${#keys[@]} -eq 0 ] && keys=(u64 uuid str-json)

mkdir -p "$work"
[ -d "$work/bbtree" ] || git clone -q https://github.com/XiangpengHao/bbtree "$work/bbtree"
[ -d "$work/bbtree-eval" ] || git clone -q https://github.com/XiangpengHao/bbtree-eval "$work/bbtree-eval"
git -C "$work/bbtree" checkout -q 6619c4e926f701b0e28f80e47c01472ab09f13ae
git -C "$work/bbtree-eval" checkout -q -f 01229ec0c07c30a89ae379de3179f4ce5b3b9361
sed "s|@B_POSITIVE@|$b_positive|" "$here/harness.patch" | git -C "$work/bbtree-eval" apply
cp "$here/b_positive.rs" "$work/bbtree-eval/src/concurrent_maps/"
(cd "$work/bbtree-eval" && cargo build --release --features blog --bin bbtree-scaling)

bin=$work/bbtree-eval/target/release/bbtree-scaling
export BPOSITIVE_DIR=$work/data
mkdir -p "$BPOSITIVE_DIR" "$work/results"
for key in "${keys[@]}"; do
    ${GUARD:-} "$bin" --entries "$entries" --threads 1 --keys "$key" --maps bbtree \
        --workloads get-half,get-half-batch,insert,insert-batch,update,scan --measure throughput --seconds 1 \
        --out "$work/results/$key-bbtree"
    ${GUARD:-} "$bin" --entries "$entries" --threads 1 --keys "$key" \
        --maps b-positive,std-btree,crossbeam-skipmap,scc-treeindex,concurrent-map,std-hash \
        --workloads get-half,insert,update,scan --measure throughput --seconds 1 --out "$work/results/$key"
    ${GUARD:-} "$bin" --entries "$entries" --threads 1 --keys "$key" --maps b-positive \
        --workloads get-half-batch --measure throughput --seconds 1 --out "$work/results/$key-b-positive-batch"
done
echo "results in $work/results"
