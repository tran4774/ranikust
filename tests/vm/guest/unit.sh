#!/usr/bin/env bash
# Runs in the T2 VM: every `#[ignore = "T2: ..."]` and `"T2c: ..."` test, as root, from the
# static test binaries in ~/rnk/bin/tests. vm.sh lists their names in
# ~/rnk/t2-tests.txt; each must run exactly once and pass.
set -euo pipefail
cd "$HOME/rnk"
mapfile -t names <t2-tests.txt
work="$(mktemp -d)"
trap 'sudo rm -rf "$work"' EXIT

declare -A want=()
for n in "${names[@]}"; do want[$n]=1; done
ran=0
for bin in bin/tests/*; do
  # Exact names only: each ignored test of the binary whose last path
  # component is listed, run with --exact, so a name that merely contains a
  # listed one never runs by accident.
  sel=()
  while IFS= read -r t; do
    [ -n "${want[${t##*::}]:-}" ] && sel+=("$t")
  done < <("$HOME/rnk/$bin" --list --ignored 2>/dev/null | sed -n 's/: test$//p')
  [ "${#sel[@]}" -gt 0 ] || continue
  out="$(cd "$work" && sudo TMPDIR="$work" "$HOME/rnk/$bin" --ignored --exact --test-threads=1 "${sel[@]}" 2>&1)" || {
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
