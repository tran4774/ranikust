# shellcheck shell=bash
# A RUN's cgroups as it reads them (ADR-0110): where ranikust gives each RUN
# cgroups of its own (the T1 leg, alone in a --privileged container's cgroup
# namespace: nested), the command reads `0::/` and its init's cgroup beside
# or above it, never ranikust's pid or the RUN's number. So --reproducible
# gives one digest at --jobs 1 and 4, build after build. Where no RUN gets a
# cgroup (the VM's ssh session), it reads ranikust's own, and only the
# builds and their digests are checked.
case_run() {
  local a b c d got
  a="$(build j1 --reproducible --jobs 1)"
  b="$(build j4 --reproducible --jobs 4)"
  c="$(build j1-again --reproducible --jobs 1)"
  d="$(build j4-again --reproducible --jobs 4)"
  expect "$b" "$a" "digest at --jobs 4 and 1"
  expect "$c" "$a" "digest of a second build at --jobs 1"
  expect "$d" "$a" "digest of a second build at --jobs 4"
  if ! grep -q 'RUN cgroups in' "$(out j4)/build.log"; then
    echo "  $CASE: no RUN cgroups here: only the builds and their digests were checked"
    return 0
  fi
  got="$(run j4 | tr '\n' ' ')"
  case "$got" in
  "0::/ 0::/../init 0::/ 0::/../init " | "0::/ 0::/../.. 0::/ 0::/../.. ") ;;
  *) fail "with RUN cgroups the RUNs read $(printf %q "$got")" ;;
  esac
}
