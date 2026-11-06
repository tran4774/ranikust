# shellcheck shell=bash
# ranikust's and the init's cgroup namespaces are out of a RUN's reach
# (ADR-0110, T24); the init shares ranikust's, so the probe tries the
# init's. Where ranikust gives a RUN cgroups (the T1 leg, a --privileged
# container): every /proc/<pid>/cgroup the RUN can read is relative to its
# own cgroup namespace, so none names ranikust's pid; a setns into the
# init's cgroup namespace is refused, as root in the RUN and from a user
# namespace it makes; and a cgroupfs it mounts in a user namespace is
# rooted at the cgroup it is in, so it shows only that cgroup and those
# below it. Elsewhere no RUN gets a cgroup and only the build is checked.
case_run() {
  local r line v vs
  build probe >/dev/null
  if ! grep -q 'RUN cgroups in' "$(out probe)/build.log"; then
    echo "  $CASE: no RUN cgroups here: only the build was checked"
    return 0
  fi
  r="$(run probe)"
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
  line="$(sed -n 's/^setns-userns=//p' <<<"$r")"
  case "$line" in
  open:* | setns:*) ;;
  unshare:* | setgroups:* | uid_map:* | gid_map:*) echo "  $CASE: no user namespace for the RUN here ($line): its setns check did not run" ;;
  *) fail "setns-userns recorded $line: $r" ;;
  esac
  line="$(sed -n 's/^mount-ns=//p' <<<"$r")"
  case "$line" in
  "refused "*) echo "  $CASE: no cgroupfs of the RUN's own here (${line#refused }): its check did not run" ;;
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
