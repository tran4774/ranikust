# probe.sh SELF SIBLING: run by a RUN of the stage SELF while the stage
# SIBLING runs (ADR-0059). Writes /report: one line per defence that gave
# way, nothing when all held.
me="$1" sib="$2" r=/report
: >"$r"
echo "$me" >"/mark-$me"
i=0
while [ "$i" -lt 30 ]; do
  # Every process seen shares this root: no sibling's process to read or
  # ptrace.
  for p in /proc/[0-9]*; do
    t="$(readlink "$p/root" 2>/dev/null)" || continue
    [ "$t" = / ] || echo "foreign root: ${p#/proc/} -> $t" >>"$r"
  done
  [ ! -e "/proc/1/root/mark-$sib" ] || echo "pid 1's root holds mark-$sib" >>"$r"
  find / -xdev -name "mark-$sib" 2>/dev/null | sed 's/^/found: /' >>"$r"
  find / -xdev -type d -path '*/builds/*/stages' 2>/dev/null | sed 's/^/stages dir: /' >>"$r"
  i=$((i + 1))
  sleep 0.1
done
# No mount, no sysctl write (T17), no raw disk through a node of its own
# (T18: every mount but /dev's binds is nodev).
mkdir -p /mnt/t && mount -t tmpfs none /mnt/t 2>/dev/null && echo "mounted a tmpfs" >>"$r"
{ echo "|/bin/true" >/proc/sys/kernel/core_pattern; } 2>/dev/null && echo "wrote core_pattern" >>"$r"
tail -n +3 /proc/partitions | while read -r maj min _ name; do
  mknod "/disk-$me" b "$maj" "$min" 2>/dev/null || continue
  if dd if="/disk-$me" of=/dev/null bs=512 count=1 2>/dev/null; then
    echo "read disk $name" >>"$r"
  else
    echo "probe $me: made a node of $name ($maj:$min), reading it refused" >&2
  fi
  rm -f "/disk-$me"
done
cat "$r" >&2
