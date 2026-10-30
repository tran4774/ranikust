# Quick smoke cases (tests/smoke/cases/<name>/): small Dockerfiles, one
# concern each, built with the `ranikust` binary in a few seconds. Shared by
# `just smoke-case` (T1, rootless podman) and the VM `quick` suite (T2).
# Sourced, not run.
#
# The caller defines:
#   rnk_run CTX OUT ARGS...  run `ranikust build` on the context directory
#                            CTX, writing an OCI layout to OUT/oci and a
#                            Docker tar to OUT/image.tar (host paths), with
#                            `--secret id=tok,src=<SMOKE_CASE_SECRET in a
#                            file>` and a workdir shared by every case; prints
#                            the build's log and returns its exit code
#   ENGINE                   `podman` or `sudo docker`: loads and runs images
#   BASE                     the alpine image the cases build FROM
#   CASES_OUT                a directory for the outputs
# then calls `smoke_cases [name...]` (no name: every case) as a plain
# command, not under `if`, `||` or `&&`, which would turn errexit off in
# the cases.

# The secret `--secret id=tok` holds. Cases check its sha256, never the
# value, so that the value can be searched for in logs and blobs.
SMOKE_CASE_SECRET=quick-s3cret
SMOKE_CASE_SECRET_SHA256=0010c18421c2d032793df4a17f2fa4021d76da9252c034d22b3a5e23e019dab8

SMOKE_CASES_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/cases" && pwd)"

fail() { echo "error: $CASE: $*" >&2; return 1; }

# build NAME [ARG...]: builds this case's context (or `--target`, build
# args, flags in ARG) into $(out NAME); prints the digest.
build() {
  local name="$1" o rc=0 log
  shift
  o="$(out "$name")"
  rm -rf "$o" && mkdir -p "$o"
  log="$(rnk_run "$SMOKE_CASES_DIR/$CASE" "$o" --build-arg "BASE=$BASE" \
    --destination "localhost/rnk-case:$CASE-$name" "$@" 2>&1)" || rc=$?
  printf '%s\n' "$log" >"$o/build.log"
  if [ "$rc" != 0 ] || ! grep -Eq '^sha256:[0-9a-f]{64}$' <<<"$log"; then
    echo "$log" >&2
    fail "build $name exited $rc" || return 1
  fi
  smoke_check_no_stub_leak "$o/oci" &&
    smoke_check_no_secret "$o/oci" "$SMOKE_CASE_SECRET" &&
    smoke_check_no_secret_in "$SMOKE_CASE_SECRET" "$o/build.log" || return 1
  grep -Eo '^sha256:[0-9a-f]{64}$' <<<"$log" | tail -1
}

out() { echo "$CASES_OUT/$CASE/$1"; }

# run NAME [CMD...]: loads the build NAME and runs it (its CMD, or CMD).
run() {
  local name="$1" tag="localhost/rnk-case:$CASE-$1"
  shift
  $ENGINE load -q -i "$(out "$name")/image.tar" >/dev/null || fail "load $name" || return 1
  $ENGINE run --rm "$tag" "$@"
}

# expect GOT WANT WHAT
expect() {
  [ "$1" = "$2" ] || fail "$3: got $(printf %q "$1"), want $(printf %q "$2")"
}

# expect_config NAME JSON: the image config of the build NAME contains JSON
# (objects by subset, everything else equal).
expect_config() {
  python3 -I "$SMOKE_CASES_DIR/../oci-config.py" "$(out "$1")/oci" "$2" || fail "config of $1"
}

# config NAME: the image config of the build NAME, as JSON.
config() {
  python3 -I "$SMOKE_CASES_DIR/../oci-config.py" "$(out "$1")/oci"
}

# Runs the named cases, or all of them; prints one line per case.
smoke_cases() {
  local names=("$@") failed=() n t0 t1
  [ ${#names[@]} -gt 0 ] || mapfile -t names < <(ls "$SMOKE_CASES_DIR")
  for n in "${names[@]}"; do
    [ -f "$SMOKE_CASES_DIR/$n/case.sh" ] || { echo "error: no case $n" >&2; return 1; }
    t0=$(date +%s.%N)
    # In the background, not under `if`: errexit is ignored in a tested
    # subshell, and a case relies on it.
    (
      CASE="$n"
      set -eo pipefail
      shopt -s inherit_errexit
      # shellcheck disable=SC1090
      source "$SMOKE_CASES_DIR/$n/case.sh"
      case_run
    ) &
    if wait $!; then
      t1=$(date +%s.%N)
      printf 'case %s: ok (%s s)\n' "$n" "$(awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.1f", b - a }')"
    else
      echo "case $n: FAILED (outputs in $CASES_OUT/$n)"
      failed+=("$n")
    fi
  done
  [ ${#failed[@]} = 0 ] || { echo "failed cases: ${failed[*]}" >&2; return 1; }
}
