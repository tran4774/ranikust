# shellcheck shell=bash
# ranikust's and the init's cgroup namespaces are out of a RUN's reach
# (ADR-0110, T24); the init shares ranikust's, so the probe tries the
# init's. Where ranikust gives a RUN cgroups (the T1 leg, a --privileged
# container): every /proc/<pid>/cgroup the RUN can read is relative to its
# own cgroup namespace, so none names ranikust's pid, and a setns into the
# init's cgroup namespace is refused. Under the default profile (ADR-0109)
# the RUN makes no user namespace. Unconfined (--seccomp=unconfined), which
# measures what a RUN that may make one can do: the setns is refused from a
# user namespace it makes too, and a cgroupfs it mounts there is rooted at
# the cgroup it is in, so it shows only that cgroup and those below it.
# Elsewhere no RUN gets a cgroup and only the build is checked.
case_run() {
  local r line
  build default >/dev/null
  if ! grep -q 'RUN cgroups in' "$(out default)/build.log"; then
    echo "  $CASE: no RUN cgroups here: only the build was checked"
    return 0
  fi
  r="$(run default)"
  check_views "$r"
  line="$(sed -n 's/^setns-userns=//p' <<<"$r")"
  case "$line" in unshare:*) ;; *) fail "the default profile let the RUN make a user namespace: setns-userns=$line" ;; esac
  line="$(sed -n 's/^mount-ns=//p' <<<"$r")"
  case "$line" in "refused unshare:"*) ;; *) fail "the default profile let the RUN make a user namespace: mount-ns=$line" ;; esac

  build unconfined --seccomp=unconfined >/dev/null
  r="$(run unconfined)"
  check_views "$r"
  line="$(sed -n 's/^setns-userns=//p' <<<"$r")"
  case "$line" in
  open:* | setns:*) ;;
  unshare:* | setgroups:* | uid_map:* | gid_map:*) echo "  $CASE: no user namespace for an unconfined RUN here ($line): its setns check did not run" ;;
  *) fail "setns-userns recorded $line: $r" ;;
  esac
  line="$(sed -n 's/^mount-ns=//p' <<<"$r")"
  case "$line" in
  "refused "*) echo "  $CASE: no cgroupfs of an unconfined RUN's own here (${line#refused }): its check did not run" ;;
  me=*)
    # The probe made no cgroup: the mount's root is the one it is in.
    local me="${line#me=}" procs
    me="${me%% *}"
    procs="$(sed -nE 's/.* procs=([^ ]*) .*/\1/p' <<<"$line")"
    case ",$procs," in *",$me,"*) ;; *) fail "a mounted cgroupfs is not rooted at the cgroup the RUN is in: $line" ;; esac
    [ "${line##* children=}" = "" ] || fail "a mounted cgroupfs shows cgroups the RUN did not make: $line"
    ;;
  *) fail "the mount probe recorded $line" ;;
  esac
  return 0
}

# check_views REPORT: every line is there, every cgroup the RUN reads is
# relative to its own cgroup namespace, and the plain setns is refused.
check_views() {
  local r="$1" line v vs
  for line in cgroups setns-plain setns-userns mount-ns; do
    grep -q "^$line=" <<<"$r" || fail "no $line line: $r"
  done
  line="$(sed -n 's/^cgroups=//p' <<<"$r")"
  case "|$line|" in *'|0::/|'*) ;; *) fail "no process reads 0::/: $r" ;; esac
  case "|$line|" in *'|0::/../init|'* | *'|0::/../..|'*) ;; *) fail "the init's cgroup is not relative: $r" ;; esac
  IFS='|' read -ra vs <<<"$line"
  for v in "${vs[@]}"; do
    case "$v" in 0::/ | 0::/../init | 0::/../..) ;; *) fail "a process reads $v: $r" ;; esac
  done
  # A refused setns, or a refused open of the namespace before it.
  line="$(sed -n 's/^setns-plain=//p' <<<"$r")"
  case "$line" in open:* | setns:*) ;; *) fail "setns-plain recorded $line: $r" ;; esac
}
