#!/usr/bin/env bash
# Tests scripts/check-static.sh with fake `readelf` and `file` on PATH.
#
# The bad cases print the line that must fail the check first, then more
# output than a pipe holds: `readelf ... | grep -q` under pipefail then ends
# with readelf's SIGPIPE (141), which an `if` reads as "no match", so a
# binary with a loader would pass. Run: `just test-release`.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir "$tmp/bin"

# The fake readelf prints, per option, the line in $FAKE_<option>, then
# 1 MiB of filler when FAKE_FLOOD is set.
cat >"$tmp/bin/readelf" <<'EOF'
#!/usr/bin/env bash
case "$*" in
  *--program-headers*) printf '%s\n' "${FAKE_PROGRAM:-  LOAD 0x0}" ;;
  *--dynamic*) printf '%s\n' "${FAKE_DYNAMIC:-There is no dynamic section in this file.}" ;;
  *--file-header*) printf '%s\n' "${FAKE_TYPE:-  Type:                              DYN (Position-Independent Executable file)}" ;;
  *--section-headers*) printf '%s\n' "${FAKE_SECTIONS:-  [30] .symtab           SYMTAB}" ;;
esac
if [ -n "${FAKE_FLOOD:-}" ]; then
  head -c 1048576 /dev/zero | tr '\0' 'x' | fold -w 100
fi
EOF
printf '#!/bin/sh\necho "$1: fake ELF"\n' >"$tmp/bin/file"
printf '#!/bin/sh\necho ranikust 0.0.0\n' >"$tmp/ranikust"
chmod +x "$tmp/bin/readelf" "$tmp/bin/file" "$tmp/ranikust"

failures=0
expect() { # expect <0|1> <message to find or -> <description> [VAR=value...]
  local want="$1" needle="$2" what="$3"
  shift 3
  local out rc=0
  out="$(env PATH="$tmp/bin:$PATH" "$@" "$root/scripts/check-static.sh" "$tmp/ranikust" 2>&1)" || rc=$?
  if { [ "$want" = 0 ] && [ "$rc" != 0 ]; } || { [ "$want" = 1 ] && [ "$rc" = 0 ]; } ||
     { [ "$needle" != - ] && ! grep -qF -- "$needle" <<<"$out"; }; then
    echo "FAIL: $what (exit $rc)"
    printf '%s\n' "$out" | head -5 | sed 's/^/    /'
    failures=$((failures + 1))
  else
    echo "ok: $what"
  fi
}

interp='      [Requesting program interpreter: /lib64/ld-linux-x86-64.so.2]'
expect 0 'ranikust 0.0.0' "a static-pie binary passes"
expect 0 'ranikust 0.0.0' "a static-pie binary passes with long readelf output" FAKE_FLOOD=1
expect 1 'dynamic loader' "a loader fails" FAKE_PROGRAM="$interp"
expect 1 'dynamic loader' "a loader fails with long readelf output" FAKE_PROGRAM="$interp" FAKE_FLOOD=1
expect 1 'shared libraries' "a NEEDED entry fails with long output" \
  FAKE_DYNAMIC=' 0x0000000000000001 (NEEDED)             Shared library: [libc.so.6]' FAKE_FLOOD=1
expect 1 'position-independent' "a non-PIE binary fails" \
  FAKE_TYPE='  Type:                              EXEC (Executable file)'
expect 1 'symbol table' "a stripped binary fails" FAKE_SECTIONS='  [29] .shstrtab STRTAB'

if [ "$failures" != 0 ]; then
  echo "$failures check-static case(s) failed"
  exit 1
fi
