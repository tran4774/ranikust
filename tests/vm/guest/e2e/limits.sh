#!/usr/bin/env bash
# Writes the CPU and memory limits a build really ran under, read from its
# cgroup (v2), so that the e2e results show every builder had the same, and
# the build's peak memory (the cgroup's memory.peak, page cache included;
# `unknown` when it cannot be read).
#
#   limits.sh self <out> <command...>   the limits of this process's cgroup
#                                       (a systemd scope) into <out> (a
#                                       directory no container reaches),
#                                       then runs command (signals passed
#                                       on to it) and appends the scope's
#                                       memory.peak; exits with the
#                                       command's status
#   limits.sh pid <pid>                 the limits of <pid>'s cgroup (a
#                                       container's first process) on
#                                       stdout, for the caller to write
#   limits.sh watch <pid>               the highest memory.peak of <pid>'s
#                                       cgroup, read every 0.2 s for as long
#                                       as that cgroup exists (a container's
#                                       goes with it), on stdout; on SIGTERM
#                                       it prints what it has and stops
set -euo pipefail
cgroup_of() { awk -F: '$1 == "0" { print $3 }' "/proc/$1/cgroup"; }
show() { # <pid>
  local cg f
  cg="$(cgroup_of "$1")"
  for f in cpu.max cpuset.cpus.effective memory.max memory.swap.max; do
    printf '%s=%s\n' "$f" "$(cat "/sys/fs/cgroup$cg/$f" 2>/dev/null || echo unknown)"
  done
}
# peak_of <cgroup>: its memory.peak, or nothing.
peak_of() {
  local v
  v="$(cat "/sys/fs/cgroup$1/memory.peak" 2>/dev/null)" || return 0
  [[ $v =~ ^[0-9]+$ ]] && echo "$v"
  return 0
}
case "${1:-}" in
  self)
    out="$2"
    show "$$" >"$out"
    shift 2
    # The command runs as a child, not by exec, so that its scope's peak
    # can be read after it; a signal for this shell goes on to it.
    "$@" &
    child=$!
    trap 'kill -TERM "$child" 2>/dev/null || true' TERM
    trap 'kill -INT "$child" 2>/dev/null || true' INT
    trap 'kill -HUP "$child" 2>/dev/null || true' HUP
    rc=0
    while :; do
      if wait "$child"; then rc=0; else rc=$?; fi
      # Reaped: rc is its status. Still there (wait cut short by a
      # signal, or a zombie): wait again.
      [ -e "/proc/$child" ] || break
    done
    p="$(peak_of "$(cgroup_of "$$")")"
    printf 'memory.peak=%s\n' "${p:-unknown}" >>"$out"
    exit "$rc"
    ;;
  pid) show "$2" ;;
  watch)
    [[ $2 =~ ^[1-9][0-9]*$ ]] || { echo "limits.sh watch: bad pid" >&2; exit 2; }
    cg="$(cgroup_of "$2" 2>/dev/null)" || cg=
    max=
    trap 'printf "memory.peak=%s\n" "${max:-unknown}"; exit 0' TERM INT
    # The root cgroup (empty or /) has no memory.peak: nothing to watch.
    if [ -n "$cg" ] && [ "$cg" != / ]; then
      while [ -e "/sys/fs/cgroup$cg/memory.peak" ]; do
        v="$(peak_of "$cg")"
        [ -z "$v" ] || { [ -n "$max" ] && [ "$v" -le "$max" ]; } || max="$v"
        sleep 0.2
      done
    fi
    printf 'memory.peak=%s\n' "${max:-unknown}"
    ;;
  *) echo "usage: limits.sh self <out> <command...> | pid <pid> | watch <pid>" >&2; exit 2 ;;
esac
