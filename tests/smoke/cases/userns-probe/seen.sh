# What a RUN of a --mode=auto build sees of the user namespace probe (#74):
# one line per trace it finds, or "seen: nothing". Writes /seen.
set -u
{
  read -r own </proc/self/uid_map
  for p in /proc/[0-9]*; do
    # A process in another user namespace reads another first line here,
    # an empty one before its map is written; no ptrace access is needed.
    m=
    { read -r m || :; } 2>/dev/null <"$p/uid_map" || continue
    [ "$m" = "$own" ] || echo "user namespace: ${p#/proc/} $(cat "$p/comm" 2>/dev/null)"
  done
  for f in /proc/self/mountinfo /proc/1/mountinfo; do
    # The mount point (field 5): a source path may name the case.
    awk -v f="$f" '$5 ~ /userns-probe|[.]ranikust-anon-/ { print "mount in " f ": " $5 }' "$f" 2>/dev/null
  done
  for f in /proc/$$/fd/*; do
    t="$(readlink "$f" 2>/dev/null)" || continue
    case "$t" in */builds/*/userns-probe* | */.ranikust-anon-*) echo "descriptor: $f -> $t" ;; esac
  done
  env | grep '^RANIKUST_' | sed 's/=.*//; s/^/variable: /'
  for d in /var/lib/ranikust/builds/*/userns-probe /var/lib/ranikust/.ranikust-anon-*; do
    [ -e "$d" ] && echo "state directory: $d"
  done
} >/seen.tmp
[ -s /seen.tmp ] || echo "seen: nothing" >/seen.tmp
mv /seen.tmp /seen
