#!/bin/sh
# Red team (ADR-0101, T7, T21): the exe, descriptors, maps, root and
# environment of every ranikust thread a RUN sees; exits 1 on any hit.
set -u
hits=
hit() {
  hits="$hits
  $*"
}
probe() {
  local t="$1" e="$1/exe" f
  readlink "$e" >/dev/null 2>&1 && hit "readlink $e: $(readlink "$e")"
  head -c 1 "$e" >/dev/null 2>&1 && hit "read $e"
  (exec 3<>"$e") 2>/dev/null && hit "open $e read-write"
  (exec 3>>"$e") 2>/dev/null && hit "open $e to append"
  "$e" version >/dev/null 2>&1 && hit "run $e"
  head -c 1 "$t/environ" >/dev/null 2>&1 && hit "read $t/environ"
  (exec 3<"$t/mem") 2>/dev/null && hit "open $t/mem"
  ls "$t/root/" >/dev/null 2>&1 && hit "list $t/root"
  for f in "$t"/fd/* "$t"/map_files/*; do
    [ -L "$f" ] || continue
    readlink "$f" >/dev/null 2>&1 && hit "readlink $f: $(readlink "$f")"
    head -c 1 "$f" >/dev/null 2>&1 && hit "read $f"
  done
}

# The control: the probes reach a dumpable process of this RUN.
sleep 60 &
ctl=$!
readlink "/proc/$ctl/exe" >/dev/null && head -c 1 "/proc/$ctl/exe" >/dev/null &&
  ls "/proc/$ctl/root/" >/dev/null && head -c 1 "/proc/$ctl/environ" >/dev/null || {
  echo "reach: the control probes fail on a process of the RUN's own" >&2
  exit 1
}
kill "$ctl"

seen=0
for d in /proc/[0-9]*; do
  case "$(cat "$d/comm" 2>/dev/null)" in ranikust | ranikust-init) ;; *) continue ;; esac
  for t in "$d" "$d"/task/*; do
    [ -d "$t" ] || continue
    seen=$((seen + 1))
    probe "$t"
  done
done
[ "$seen" -gt 0 ] || { echo "reach: no process of ranikust's seen" >&2; exit 1; }

for f in /proc/$$/fd/*; do
  t="$(readlink "$f" 2>/dev/null)" || continue
  case "$t" in *ranikust* | *mountinfo* | */exe) hit "own descriptor ${f##*/} -> $t" ;; esac
done
env | grep -q '^RANIKUST_' && hit "own environment: $(env | grep '^RANIKUST_' | cut -c1-60)"

if [ -n "$hits" ]; then
  echo "reach: a RUN reached ranikust's binary or descriptors:$hits" >&2
  exit 1
fi
echo "reach: $seen threads of ranikust's out of reach" | tee /reached
