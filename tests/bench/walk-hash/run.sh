#!/usr/bin/env bash
# Q-038 benchmark: every walker × thread count × hash, interleaved, then
# the hash alone on in-memory files. Page cache warm (one untimed walk
# first): a user cannot drop caches.
#
#   tests/bench/walk-hash/run.sh <root> <out-dir> [reps]
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$1 out=$2 reps=${3:-7}
mkdir -p "$out"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$here/../../../target/bench-q038/build}
cargo build --release --locked --manifest-path "$here/Cargo.toml" -q
bin=$CARGO_TARGET_DIR/release/walk-hash-bench
{
  uname -r; nproc; lscpu | sed -n 's/^Model name: *//p'
  findmnt -no FSTYPE,SOURCE -T "$root"
} > "$out/env.txt"
"$bin" walk fd "$root" 8 xxh3 > /dev/null
: > "$out/walk.jsonl"
for rep in $(seq "$reps"); do
  for threads in 1 2 4 8 16 32; do
    for hash in none xxh3 highway sha256; do
      for walker in fd jwalk; do
        "$bin" walk "$walker" "$root" "$threads" "$hash" >> "$out/walk.jsonl"
      done
    done
  done
  echo "rep $rep done" >&2
done
: > "$out/mem.jsonl"
for threads in 1 8; do
  for hash in xxh3 highway sha256; do
    "$bin" mem "$root" "$threads" "$hash" >> "$out/mem.jsonl"
  done
done
python3 -I "$here/summarize.py" "$out"
