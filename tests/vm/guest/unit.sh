#!/usr/bin/env bash
# Runs in the T2 VM: every `#[ignore = "T2: ..."]` test, as root, from the
# static test binaries in ~/rnk/bin/tests. vm.sh lists their names in
# ~/rnk/t2-tests.txt; each must run exactly once and pass.
set -euo pipefail
cd "$HOME/rnk"
mapfile -t names <t2-tests.txt
work="$(mktemp -d)"
trap 'sudo rm -rf "$work"' EXIT

ran=0
for bin in bin/tests/*; do
  # libtest takes several filters; --ignored runs only the ignored tests.
  out="$(cd "$work" && sudo TMPDIR="$work" "$HOME/rnk/$bin" --ignored --test-threads=1 "${names[@]}" 2>&1)" || {
    echo "$out"
    echo "error: $(basename "$bin") failed" >&2
    exit 1
  }
  n="$(sed -nE 's/^test result: ok\. ([0-9]+) passed.*/\1/p' <<<"$out" | awk '{ s += $1 } END { print s + 0 }')"
  if [ "$n" -gt 0 ]; then
    echo "$out" | grep -E '^test .* \.\.\. '
    ran=$((ran + n))
  fi
done
echo "T2 tests: $ran passed of ${#names[@]} listed"
[ "$ran" = "${#names[@]}" ] || { echo "error: expected ${#names[@]} T2 tests to run" >&2; exit 1; }
