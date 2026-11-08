# Runs in each pod of guest/k8s.sh, under the image's static busybox: prints
# what the pod got (the suite's controls), then runs ranikust with the pod's
# arguments.
#   RNK_STATE             a volume at the state directory: made 0700 first,
#                         as ranikust wants it (a fresh emptyDir is 0777)
#   RNK_MEASURE_PROC_SYS  try writing /proc/sys/kernel/core_pattern's own
#                         value, and print what happened
b=/bin/busybox
st() { $b awk -v k="$1:" '$1 == k { print $2 }' /proc/self/status; }
echo "pod: uid $($b id -u) gid $($b id -g)"
echo "pod: apparmor $($b cat /proc/self/attr/current 2>/dev/null || echo none)"
echo "pod: uid_map $($b awk '{ printf "%s %s %s;", $1, $2, $3 }' /proc/self/uid_map)"
echo "pod: seccomp $(st Seccomp) no_new_privs $(st NoNewPrivs) CapEff $(st CapEff)"
# The mount /proc/sys is on: its own (a masked /proc: a read-only bind), else
# /proc. Its options are what ranikust's statvfs reads.
m="$($b awk '$5 == "/proc/sys" { o = $6 } END { print o }' /proc/self/mountinfo)"
if [ -n "$m" ]; then
  echo "pod: /proc/sys on /proc/sys $m"
else
  echo "pod: /proc/sys on /proc $($b awk '$5 == "/proc" { o = $6 } END { print o }' /proc/self/mountinfo)"
fi
if [ -n "${RNK_MEASURE_PROC_SYS:-}" ]; then
  v="$($b cat /proc/sys/kernel/core_pattern)"
  if e="$( (printf '%s\n' "$v" >/proc/sys/kernel/core_pattern) 2>&1)"; then
    echo "pod: a write of kernel/core_pattern's own value: done"
  else
    echo "pod: a write of kernel/core_pattern's own value: refused: ${e##*: }"
  fi
fi
[ -z "${RNK_STATE:-}" ] || $b chmod 0700 "$RNK_STATE" || exit 120
# The context on the pod's volume: kaniko mode on / empties the image's root.
$b mkdir -p /work/ctx && $b cp -R /ctx/. /work/ctx/ || exit 121
exec /ranikust/bin/ranikust "$@"
