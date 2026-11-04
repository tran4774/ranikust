#!/bin/bash
# A hostile RUN step (ADR-0051): tries to read ranikust's state and
# credentials while kaniko mode builds on the container's `/`. Prints one
# line per vector, `VEC <id> blocked|LEAK|INVALID [detail]`, and always
# exits 0, so that the build goes on and secrets.sh reads every line.
#
#   seed     a RUN whose layer holds MARK, so the marker is in ranikust's
#            state (an encoded layer) once this step is snapshotted
#   unseed   removes it from the root again
#   spawn    leaves `sleep 3001 &`, a `setsid sleep 3002 &` and a
#            double-forked `sleep 3003` behind
#   swap     replaces the --secret file with a symlink to /proc/self/comm
#   V1..V13  the vectors (see secrets.sh)
set -u
MARK=MARKER-STATE-4242
# REGISTRY_TOKEN's value, and DOCKER_AUTH_CONFIG's auth: base64 of
# `user:tok-RNK-4242`.
TOKENS=(tok-RNK-4242 dXNlcjp0b2stUk5LLTQyNDI)
ARGV_SECRET=argv-secret-4242
# The --secret file's content (secrets.sh), and where the container has it.
FILE_SECRET=file-secret-RNK-5151
SECRET_SRC=/ranikust/out/sec/s
me=$$

out() { echo "VEC $*"; }

# Pids whose comm is ranikust.
rnk_pids() {
  local d
  for d in /proc/[0-9]*; do
    [ "$(cat "$d/comm" 2>/dev/null)" = ranikust ] && echo "${d#/proc/}"
  done
}

# This script's own process and its children (pipes, subshells): never a
# target.
mine() {
  local p=$1 q=$p
  while [ -n "$q" ] && [ "$q" != 0 ] && [ "$q" != 1 ]; do
    [ "$q" = "$me" ] && return 0
    q=$(awk '{print $4}' "/proc/$q/stat" 2>/dev/null) || return 1
  done
  return 1
}

case "${1:-}" in
seed) echo "$MARK" >/seed-marker ;;
unseed) rm -f /seed-marker ;;
swap)
  # The --secret file is on a volume this RUN may write: point it at a
  # file ranikust would read as itself if it opened the path again.
  rm -f "$SECRET_SRC" && ln -s /proc/self/comm "$SECRET_SRC"
  ;;
spawn)
  sleep 3001 &
  setsid sleep 3002 &
  (setsid sh -c 'sleep 3003 &' &)
  ;;
V1)
  # Anything under the state directory: a name, or the marker. The
  # store's marker of a build in progress (ADR-0056) holds only the PID
  # namespace and the boot id, both readable by a RUN anyway: it must not
  # hold anything else.
  b=/var/lib/ranikust/.ranikust-building
  n=$(find /var/lib/ranikust -mindepth 1 ! -path "$b" 2>/dev/null | wc -l)
  if [ "$n" -gt 0 ]; then
    out V1 LEAK "$n entries"
  # The whole file must equal what this RUN reads itself (or be empty when
  # ranikust could not read it): `grep -x` would accept extra lines.
  elif [ -e "$b" ] && [ -s "$b" ] &&
    [ "$(cat "$b")" != "$(readlink /proc/self/ns/pid) $(cat /proc/sys/kernel/random/boot_id)" ]; then
    out V1 LEAK "the marker holds $(head -c 80 "$b" | tr -c '[:print:]' '?')"
  else
    out V1 blocked
  fi
  ;;
V2)
  pids=$(rnk_pids)
  [ -n "$pids" ] || { out V2 INVALID "no ranikust process seen"; exit 0; }
  # Listing /proc/<ranikust>/fd shows descriptor numbers only: a
  # non-dumpable process's /proc entries belong to root, and a RUN is root.
  # What leaks is a target (readlink) or content (open), both behind the
  # ptrace check.
  hit=
  for p in $pids; do
    for f in "/proc/$p/fd/"*; do
      [ -e "$f" ] || [ -L "$f" ] || continue
      readlink -- "$f" >/dev/null 2>&1 && hit="readlink $f"
      head -c1 -- "$f" >/dev/null 2>&1 && hit="opened $f"
    done
  done
  for f in /proc/[0-9]*/fd/*; do
    p=${f#/proc/}; p=${p%%/*}
    mine "$p" && continue
    [ -f "$f" ] || continue
    if { zcat -f -- "$f" 2>/dev/null || cat -- "$f" 2>/dev/null; } | grep -qs -- "$MARK"; then
      hit="marker through $f"
    fi
  done
  if [ -n "$hit" ]; then out V2 LEAK "$hit"; else out V2 blocked; fi
  ;;
V3)
  hit=
  for p in $(rnk_pids); do
    for t in "${TOKENS[@]}"; do
      tr '\0' '\n' <"/proc/$p/environ" 2>/dev/null | grep -qs -- "$t" && hit="ranikust $p environ"
    done
  done
  if [ -n "$hit" ]; then out V3 LEAK "$hit"; else out V3 blocked; fi
  ;;
V4)
  hit=
  for t in "${TOKENS[@]}"; do env | grep -qs -- "$t" && hit=own-env; done
  if [ -n "$hit" ]; then out V4 LEAK; else out V4 blocked; fi
  ;;
V5)
  left=
  for d in /proc/[0-9]*; do
    c=$(tr '\0' ' ' <"$d/cmdline" 2>/dev/null)
    case "$c" in "sleep 3001 " | "sleep 3002 " | "sleep 3003 ") left="$left ${d#/proc/}" ;; esac
  done
  if [ -n "$left" ]; then out V5 LEAK "alive:$left"; else out V5 blocked; fi
  ;;
V6)
  if unshare -Ur true 2>/dev/null; then
    r2=$(unshare -Ur bash "$0" V2)
    r3=$(unshare -Ur bash "$0" V3)
    case "$r2$r3" in *LEAK*) out V6 LEAK "$r2 / $r3" ;; *) out V6 blocked "user namespace allowed; $r2 / $r3" ;; esac
  else
    out V6 blocked "unshare -Ur refused"
  fi
  ;;
V7)
  hit=
  for d in /proc/[0-9]*; do
    p=${d#/proc/}
    mine "$p" && continue
    for t in "${TOKENS[@]}"; do
      tr '\0' ' ' <"$d/cmdline" 2>/dev/null | grep -qs -- "$t" && hit="$p cmdline"
    done
  done
  if [ -n "$hit" ]; then out V7 LEAK "$hit"; else out V7 blocked; fi
  ;;
V8)
  # Expected: a --build-arg value on ranikust's command line is readable.
  hit=
  for p in $(rnk_pids); do
    tr '\0' ' ' <"/proc/$p/cmdline" 2>/dev/null | grep -qs -- "$ARGV_SECRET" && hit="$p cmdline"
  done
  if [ -n "$hit" ]; then out V8 LEAK "$hit"; else out V8 blocked; fi
  ;;
V9)
  # Other processes than ranikust and this script holding the tokens.
  hit=
  rnk=" $(rnk_pids | tr '\n' ' ') "
  for d in /proc/[0-9]*; do
    p=${d#/proc/}
    mine "$p" && continue
    case "$rnk" in *" $p "*) continue ;; esac
    for t in "${TOKENS[@]}"; do
      tr '\0' '\n' <"$d/environ" 2>/dev/null | grep -qs -- "$t" &&
        hit="$p ($(cat "$d/comm" 2>/dev/null)) environ"
    done
  done
  if [ -n "$hit" ]; then out V9 LEAK "$hit"; else out V9 blocked; fi
  ;;
V10)
  hit=
  for p in $(rnk_pids); do
    if exec 3<"/proc/$p/mem" 2>/dev/null; then
      exec 3<&-
      hit="opened /proc/$p/mem"
    fi
  done
  if [ -n "$hit" ]; then out V10 LEAK "$hit"; else out V10 blocked; fi
  ;;
V11)
  # ranikust's own capabilities never exceed the RUN's (ADR-0078): with
  # --cap-add SYS_PTRACE it would otherwise open paths a RUN planted with a
  # capability the RUN was denied. Capabilities are per thread: every
  # thread's /proc/<pid>/task/<tid>/status, readable whatever
  # PR_SET_DUMPABLE says.
  pids=$(rnk_pids)
  [ -n "$pids" ] || { out V11 INVALID "no ranikust process seen"; exit 0; }
  hit=
  seen=0
  for p in $pids; do
    for s in "/proc/$p/task/"*/status; do
      [ -r "$s" ] || continue
      seen=$((seen + 1))
      for k in CapInh CapPrm CapEff CapBnd CapAmb; do
        a=$(sed -n "s/^$k:[[:space:]]*//p" "$s")
        b=$(sed -n "s/^$k:[[:space:]]*//p" /proc/self/status)
        [ -n "$a" ] || continue
        [ $((0x$a & ~0x$b)) -eq 0 ] || hit="$hit ${s#/proc/} $k ranikust=$a run=$b"
      done
    done
  done
  if [ "$seen" = 0 ]; then
    out V11 INVALID "no thread of ranikust readable"
  elif [ -n "$hit" ]; then
    out V11 LEAK "$hit"
  else
    out V11 blocked "$seen threads"
  fi
  ;;
V12)
  # An earlier RUN (swap) replaced the --secret file with a symlink: this
  # RUN must still get the bytes ranikust read at start-up.
  case "${S-}" in
    "$FILE_SECRET") out V12 blocked ;;
    "") out V12 INVALID "no secret variable" ;;
    *) out V12 LEAK "got $(printf %s "$S" | head -c 40 | tr -c '[:print:]' '?')" ;;
  esac
  ;;
V13)
  # ADR-0101: the sealed copy's exe, ptrace and process_vm_writev, at Yama 0.
  pids=$(rnk_pids)
  [ -n "$pids" ] || { out V13 INVALID "no ranikust process seen"; exit 0; }
  case "$(uname -m)" in
    x86_64) nr="101 311" ;;
    aarch64) nr="117 271" ;;
    *) out V13 INVALID "no syscall numbers for $(uname -m)"; exit 0 ;;
  esac
  hit=
  seen=0
  for p in $pids; do
    for t in "/proc/$p" "/proc/$p/task/"*; do
      [ -d "$t" ] || continue
      seen=$((seen + 1))
      e="$t/exe"
      readlink -- "$e" >/dev/null 2>&1 && hit="$hit readlink:$e"
      head -c1 -- "$e" >/dev/null 2>&1 && hit="$hit read:$e"
      (exec 3<>"$e") 2>/dev/null && hit="$hit rw:$e"
      "$e" version >/dev/null 2>&1 && hit="$hit run:$e"
    done
    # EFAULT from an unmapped address means the access check passed.
    # shellcheck disable=SC2086
    r=$(perl -e '
      my ($ptrace, $pvw, $pid) = map { 0 + $_ } @ARGV;
      print "ptrace-seize " if syscall($ptrace, 0x4206, $pid, 0, 0) == 0;
      my $buf = "x";
      my $local = pack("QQ", unpack("Q", pack("p", $buf)), 1);
      my $remote = pack("QQ", 0x1000, 1);
      my $r = syscall($pvw, $pid, $local, 1, $remote, 1, 0);
      print "process_vm_writev:", $r >= 0 ? "wrote" : 0 + $!, " " if $r >= 0 || !$!{EPERM};
    ' $nr "$p" 2>&1)
    [ -z "$r" ] || hit="$hit $p:$r"
  done
  if [ -n "$hit" ]; then
    out V13 LEAK "$hit"
  else
    out V13 blocked "$seen threads, yama $(cat /proc/sys/kernel/yama/ptrace_scope 2>/dev/null || echo none)"
  fi
  ;;
*) echo "usage: $0 seed|unseed|spawn|swap|V1..V13" >&2 ;;
esac
exit 0
