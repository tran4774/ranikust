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
# One workdir for every case: its store keeps the pulled base layers, and
# the `run` case's cache mount outlives a build. Kept for the next run.
WORK="$CASES_OUT/.work"
SECRET="$CASES_OUT/.secret"
# ranikust refuses a state directory others can write to.
mkdir -p "$WORK" && chmod 700 "$WORK"
printf %s "$SMOKE_CASE_SECRET" >"$SECRET"

# The workdir is a volume: overlay refuses an upper directory on the
# container's own overlay root. RANIKUST_CASE_MODE=kaniko builds in kaniko
# mode instead, in a default (unprivileged) container where root may not
# mount, so RUN runs `Direct` in a chroot (ADR-0043), on a workdir and a
# state directory of its own.
PODMAN_ARGS=(--privileged)
MODE_ARGS=()
if [ "${RANIKUST_CASE_MODE:-}" = kaniko ]; then
  PODMAN_ARGS=()
  WORK="$CASES_OUT/.kaniko-work"
  STATE="$CASES_OUT/.kaniko-state"
  mkdir -p "$WORK" "$STATE" && chmod 700 "$WORK" "$STATE"
  PODMAN_ARGS=(-v "$STATE:/s")
  MODE_ARGS=(--mode=kaniko --state-dir /s)
fi

rnk_run() {
  local ctx="$1" o="$2"
  shift 2
  podman run --rm "${PODMAN_ARGS[@]}" -v "$RNK:/ranikust:ro" -v "$ctx:/ctx:ro" -v "$WORK:/w" \
    -v "$o:/out" -v "$SECRET:/secret:ro" "$BASE" \
    /ranikust --context /ctx --workdir /w "${MODE_ARGS[@]}" --no-push \
    --oci-layout-path /out/oci --tar-path /out/image.tar --secret id=tok,src=/secret "$@"
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
