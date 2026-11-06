# The reach of a RUN's seccomp profile (ADR-0103, ADR-0109): every process
# of the RUN's PID namespace, its init and processes left in the background
# or in a session of their own included, runs under the same filters, the
# profile among them, without no_new_privs; a setuid-root program run by
# another user gets Docker's 14 capabilities at most and keeps those
# filters; and the profile never changes a layer: one --reproducible digest
# without the flag, with --seccomp=unconfined and with an allow-all profile
# file. In the T1 kaniko leg a RUN runs on the container's `/` under the
# container's own profile (the `seccomp` case): nothing of ranikust's here.

case_run() {
  if [ "${RANIKUST_CASE_MODE:-}" = kaniko ]; then
    return 0
  fi
  local procs n filters suid f a b c
  build scope >/dev/null
  procs="$(run scope cat /procs)"
  n="$(run scope cat /nprocs)"
  [ "$n" -ge 5 ] || fail "processes seen in the RUN: $n, want the init, the shell, two sleeps and more"
  [ "$(wc -l <<<"$procs")" = 1 ] || fail "processes under different filters: $procs"
  grep -Eq '^NoNewPrivs:0 Seccomp:2 Seccomp_filters:[1-9][0-9]* $' <<<"$procs" ||
    fail "the RUN's processes: $procs"
  filters="${procs##*Seccomp_filters:}"
  filters="${filters% }"
  suid="$(run scope cat /tmp/suid)"
  grep -Eq '(^|;)Uid: [1-9][0-9]* 0 0 0;' <<<"$suid" || fail "the setuid program did not run as root: $suid"
  for f in CapPrm CapEff CapBnd; do
    grep -q "$f: 00000000a80425fb;" <<<"$suid" || fail "$f of the setuid program: $suid"
  done
  grep -q "NoNewPrivs: 0;Seccomp: 2;Seccomp_filters: $filters;" <<<"$suid" ||
    fail "the setuid program's filters: $suid; the RUN's: $procs"
  a="$(build same --target same --reproducible)"
  b="$(build same-unconfined --target same --reproducible --seccomp=unconfined)"
  c="$(build same-allow --target same --reproducible \
    "--seccomp=$(rnk_ctx "$SMOKE_CASES_DIR/$CASE")/allow.json")"
  expect "$b" "$a" "digest under --seccomp=unconfined"
  expect "$c" "$a" "digest under an allow-all profile file"
}
