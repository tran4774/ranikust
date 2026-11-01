#!/usr/bin/env bash
# `just test-container`: sends the current branch (committed state only) to
# the disposable test machine through the lane helper and runs
# tests/container/lane.sh there (ADR-0063). Nothing privileged runs here.
#
#   RANIKUST_LANE   the lane helper (default: rnk-vps-lane), which must
#                   offer `sync <ref>` and `run <ref> '<command>'` and exit
#                   with the command's status
set -euo pipefail

die() { echo "error: $*" >&2; exit 1; }

suites=("$@")
[ "${#suites[@]}" -gt 0 ] || suites=(unit-c)
for s in "${suites[@]}"; do
  case "$s" in unit-c) ;; *) die "unknown suite $s (known: unit-c)" ;; esac
done

LANE="${RANIKUST_LANE:-rnk-vps-lane}"
command -v "$LANE" >/dev/null ||
  die "no lane helper ($LANE); on a disposable machine with rootful Docker, run RANIKUST_CONTAINER_LANE=1 tests/container/lane.sh instead"

cd "$(dirname "$0")/../.."
ref="$(git branch --show-current)"
[ -n "$ref" ] || ref="$(git rev-parse HEAD)"
if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "warning: uncommitted changes are not sent; the lane runs $ref as committed" >&2
fi
mkdir -p target/container
"$LANE" sync "$ref"
set +e
"$LANE" run "$ref" "RANIKUST_CONTAINER_LANE=1 bash tests/container/lane.sh ${suites[*]}" 2>&1 |
  tee target/container/last.log
rc=${PIPESTATUS[0]}
set -e
echo "test-container: exit $rc (log: target/container/last.log)"
exit "$rc"
