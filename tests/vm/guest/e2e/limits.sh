#!/usr/bin/env bash
# Writes the CPU and memory limits a build really ran under, read from its
# cgroup (v2), so that the e2e results show every builder had the same.
#
#   limits.sh self <out> <command...>   the limits of this process's cgroup
#                                       (a systemd scope), then exec command
#   limits.sh pid <pid> <out>           the limits of <pid>'s cgroup (a
#                                       container's first process)
set -euo pipefail
write() { # <pid> <out>
  local cg f
  cg="$(awk -F: '$1 == "0" { print $3 }' "/proc/$1/cgroup")"
  for f in cpu.max cpuset.cpus.effective memory.max memory.swap.max; do
    printf '%s=%s\n' "$f" "$(cat "/sys/fs/cgroup$cg/$f" 2>/dev/null || echo unknown)"
  done >"$2"
}
case "${1:-}" in
  self)
    write "$$" "$2"
    shift 2
    exec "$@"
    ;;
  pid) write "$2" "$3" ;;
  *) echo "usage: limits.sh self <out> <command...> | pid <pid> <out>" >&2; exit 2 ;;
esac
