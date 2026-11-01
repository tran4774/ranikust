#!/usr/bin/env bash
# Writes the CPU and memory limits a build really ran under, read from its
# cgroup (v2), so that the e2e results show every builder had the same, and
# the build's peak memory (the cgroup's memory.peak, page cache included).
#
#   limits.sh self <out> <command...>   the limits of this process's cgroup
#                                       (a systemd scope) into <out> (a
#                                       directory no container reaches),
#                                       then runs command and appends the
#                                       scope's memory.peak; exits with the
#                                       command's status
#   limits.sh pid <pid>                 the limits of <pid>'s cgroup (a
#                                       container's first process) on
#                                       stdout, for the caller to write
#   limits.sh watch <pid>               the highest memory.peak of <pid>'s
#                                       cgroup, read every 0.2 s until <pid>
#                                       is gone, on stdout (a container's
#                                       cgroup goes with it, so it is
#                                       sampled while it runs)
set -euo pipefail
cgroup_of() { awk -F: '$1 == "0" { print $3 }' "/proc/$1/cgroup"; }
show() { # <pid>
  local cg f
  cg="$(cgroup_of "$1")"
  for f in cpu.max cpuset.cpus.effective memory.max memory.swap.max; do
    printf '%s=%s\n' "$f" "$(cat "/sys/fs/cgroup$cg/$f" 2>/dev/null || echo unknown)"
  done
}
case "${1:-}" in
  self)
    out="$2"
    show "$$" >"$out"
    shift 2
    rc=0
    "$@" || rc=$?
    printf 'memory.peak=%s\n' "$(cat "/sys/fs/cgroup$(cgroup_of "$$")/memory.peak" 2>/dev/null || echo unknown)" >>"$out"
    exit "$rc"
    ;;
  pid) show "$2" ;;
  watch)
    [[ $2 =~ ^[1-9][0-9]*$ ]] || { echo "limits.sh watch: bad pid" >&2; exit 2; }
    cg="$(cgroup_of "$2" 2>/dev/null)" || { echo "memory.peak=unknown"; exit 0; }
    max=0
    while [ -d "/proc/$2" ]; do
      v="$(cat "/sys/fs/cgroup$cg/memory.peak" 2>/dev/null || echo 0)"
      [ "$v" -le "$max" ] || max="$v"
      sleep 0.2
    done
    v="$(cat "/sys/fs/cgroup$cg/memory.peak" 2>/dev/null || echo 0)"
    [ "$v" -le "$max" ] || max="$v"
    printf 'memory.peak=%s\n' "$max"
    ;;
  *) echo "usage: limits.sh self <out> <command...> | pid <pid> | watch <pid>" >&2; exit 2 ;;
esac
