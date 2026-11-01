#!/usr/bin/env bash
# Writes the CPU and memory limits a build really ran under, read from its
# cgroup (v2), so that the e2e results show every builder had the same, and
# the build's peak memory (the cgroup's memory.peak, page cache included;
# `unknown` when it cannot be read).
#
#   limits.sh self <out> <command...>   the limits of this process's cgroup
#                                       (a systemd scope) into <out> (a
#                                       directory no container reaches),
#                                       then runs command (the first INT or
#                                       HUP passed on to it) and appends the
#                                       scope's memory.peak; exits with the
#                                       command's status
#   limits.sh pid <pid>                 the limits of <pid>'s cgroup (a
#                                       container's first process) on
#                                       stdout, for the caller to write
#   limits.sh watch <pid> <container id>
#                                       the highest memory.peak of <pid>'s
#                                       cgroup, which must be that
#                                       container's (docker-<id>.scope or
#                                       /docker/<id>), read every 0.2 s for
#                                       as long as it exists (a container's
#                                       goes with it), on stdout; on SIGTERM
#                                       it prints what it has and stops.
#                                       Needs no root: /proc/<pid>/cgroup
#                                       and memory.peak are world-readable
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
    # can be read after it. Job control gives it a process group of its
    # own, so that INT and QUIT are not ignored as for a plain `&` (an
    # ignored signal cannot be restored, measured in the VM 2026-11-01).
    # A terminal's INT or HUP then reaches this shell only, which passes
    # the first one on: ranikust reads a second signal as "stop at once".
    # TERM is not passed on: a scope's TERM (systemd stopping it) already
    # reaches every process in it, ranikust included.
    child='' pending='' passed='' signalled=''
    # Called from the traps below.
    # shellcheck disable=SC2329
    pass() { # <signal>
      signalled=1
      [ -z "$passed" ] || return 0
      if [ -z "$child" ]; then pending="$1"; return 0; fi
      passed=1
      kill -"$1" "$child" 2>/dev/null || true
    }
    trap 'pass INT' INT
    trap 'pass HUP' HUP
    trap 'signalled=1' TERM
    set -m
    "$@" &
    child=$!
    set +m
    if [ -n "$pending" ]; then
      passed=1
      kill -"$pending" "$child" 2>/dev/null || true
    fi
    rc=0
    wait "$child" || rc=$?
    # A trapped signal cuts wait short (status > 128): wait again, until
    # the child's own status; 127 means it was reaped already (keep rc).
    while [ -n "$signalled" ] && [ "$rc" -gt 128 ]; do
      signalled=
      r=0
      wait "$child" || r=$?
      [ "$r" != 127 ] || break
      rc="$r"
    done
    p="$(peak_of "$(cgroup_of "$$")")"
    printf 'memory.peak=%s\n' "${p:-unknown}" >>"$out"
    exit "$rc"
    ;;
  pid) show "$2" ;;
  watch)
    [[ ${2:-} =~ ^[1-9][0-9]*$ ]] || { echo "limits.sh watch: bad pid" >&2; exit 2; }
    [[ ${3:-} =~ ^[0-9a-f]{64}$ ]] || { echo "limits.sh watch: bad container id" >&2; exit 2; }
    cg="$(cgroup_of "$2" 2>/dev/null)" || cg=
    max=
    trap 'printf "memory.peak=%s\n" "${max:-unknown}"; exit 0' TERM INT
    # Only that container's cgroup: a pid reused by now names another one,
    # which is left alone (unknown).
    case "$cg" in
      */docker-"$3".scope | */docker/"$3") ;;
      *) cg= ;;
    esac
    if [ -n "$cg" ]; then
      while [ -e "/sys/fs/cgroup$cg/memory.peak" ]; do
        v="$(peak_of "$cg")"
        [ -z "$v" ] || { [ -n "$max" ] && [ "$v" -le "$max" ]; } || max="$v"
        sleep 0.2
      done
    fi
    printf 'memory.peak=%s\n' "${max:-unknown}"
    ;;
  *) echo "usage: limits.sh self <out> <command...> | pid <pid> | watch <pid> <container id>" >&2; exit 2 ;;
esac
