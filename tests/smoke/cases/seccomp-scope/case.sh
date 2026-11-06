# The reach of a RUN's seccomp profile (ADR-0103, ADR-0109): every process
# of the RUN's PID namespace, its init and processes left in the background
# or in a session of their own included, runs under the same filters
# without no_new_privs (the `seccomp` case shows the profile among a RUN's
# filters); a setuid-root program run by another user gets exactly Docker's
# 14 capabilities and keeps those filters; and the profile never changes a
# layer: one --reproducible digest without the flag, with
# --seccomp=unconfined and with an allow-all profile file. In the T1 kaniko
# leg a RUN runs on the container's `/` under the container's own profile
# (the `seccomp` case): nothing of ranikust's here.

case_run() {
  if [ "${RANIKUST_CASE_MODE:-}" = kaniko ]; then
    return 0
  fi
  local procs sh bg sid shs seen filters suid f a b c
  build scope >/dev/null
  procs="$(run scope cat /procs)"
  sh="$(run scope cat /sh)"
  bg="$(run scope cat /bg)"
  sid="$(run scope cat /sid)"
  grep -q '^1 ' <<<"$procs" || fail "no init among the RUN's processes: $procs"
  shs="$(awk -v p="$sh" '$1 == p { print $2 }' <<<"$procs")"
  [ -n "$shs" ] || fail "no shell $sh among the RUN's processes: $procs"
  awk -v p="$bg" -v s="$shs" '$1 == p && $2 == s && p != s { ok = 1 } END { exit !ok }' <<<"$procs" ||
    fail "the background process $bg, in the shell's session $shs: $procs"
  awk -v p="$sid" '$1 == p && $2 == p { ok = 1 } END { exit !ok }' <<<"$procs" ||
    fail "the process $sid, in a session of its own: $procs"
  seen="$(cut -d' ' -f3- <<<"$procs" | sort -u)"
  [ "$(wc -l <<<"$seen")" = 1 ] || fail "processes under different filters: $procs"
  grep -Eq '^NoNewPrivs:0 Seccomp:2 Seccomp_filters:[1-9][0-9]* $' <<<"$seen" ||
    fail "the RUN's processes: $procs"
  filters="${seen##*Seccomp_filters:}"
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
