#!/usr/bin/env bash
# The quick smoke cases (tests/smoke/cases) at tier T1: the static ranikust
# binary $1 runs in a `--privileged` rootless podman container, as in `just
# smoke`, and podman loads and runs what it built. Further arguments name
# cases; none runs them all. Run through `just smoke-case`.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
RNK="$(realpath "$1")"
shift
TARGET="${CARGO_TARGET_DIR:-$PWD/target}"

source tests/smoke/lib.bash
source tests/smoke/cases.bash

BASE=docker.io/library/alpine:3.20
ENGINE=podman
CASES_OUT="$TARGET/smoke/cases"
# One state directory for every case: its store keeps the pulled base
# layers, and the `run` case's cache mount outlives a build. Kept for the
# next run.
WORK="$CASES_OUT/.state"
SECRET="$CASES_OUT/.secret"
# ranikust refuses a state directory others can write to.
mkdir -p "$WORK" && chmod 700 "$WORK"
printf %s "$SMOKE_CASE_SECRET" >"$SECRET"

# The state directory is a volume at the default path: overlay refuses an
# upper directory on the container's own overlay root (ADR-0050).
# RANIKUST_CASE_MODE=kaniko builds in kaniko mode instead, in a default
# (unprivileged) container where root may not mount: on the container's
# `/`, as kaniko, with `RUN` run `Direct`, its state directory a volume of
# its own. There the root is the container's `/`, so the binary, the
# context, the outputs and the secret sit under `/ranikust`, which kaniko
# mode never snapshots or empties, as kaniko's `/kaniko`; elsewhere a case
# copying to `/ctx` would meet the context's mount.
PODMAN_ARGS=(--privileged)
MODE_ARGS=()
IN=
if [ "${RANIKUST_CASE_MODE:-}" = kaniko ]; then
  PODMAN_ARGS=()
  WORK="$CASES_OUT/.kaniko-state"
  mkdir -p "$WORK" && chmod 700 "$WORK"
  MODE_ARGS=(--mode=kaniko)
  IN=/ranikust
fi

rnk_run() {
  local ctx="$1" o="$2"
  shift 2
  podman run --rm "${PODMAN_ARGS[@]}" -v "$RNK:$IN/bin/ranikust:ro" -v "$ctx:$IN/ctx:ro" \
    -v "$WORK:/var/lib/ranikust" -v "$o:$IN/out" -v "$SECRET:$IN/secret:ro" "$BASE" \
    "$IN/bin/ranikust" --context "$IN/ctx" "${MODE_ARGS[@]}" --no-push \
    --oci-layout-path "$IN/out/oci" --tar-path "$IN/out/image.tar" \
    --secret "id=tok,src=$IN/secret" "$@"
}

# Not `smoke_cases || rc=$?`: that would turn errexit off inside every case.
set +e
smoke_cases "$@"
rc=$?
set -e
# The images the cases loaded; their outputs stay under $CASES_OUT.
podman images --format '{{.Repository}}:{{.Tag}}' | grep '^localhost/rnk-case:' |
  xargs -r podman rmi -f >/dev/null
exit "$rc"
