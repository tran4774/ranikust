#!/usr/bin/env bash
# Checks that the x86_64 musl release binary is what ranikust ships
# (ADR-0020): static-pie, no program interpreter, no shared library, a symbol
# table kept for readable panic backtraces, and that it runs.
#
# Used by `just build-static`, CI's `static-build` job and the release
# workflow (ADR-0089), so the three cannot drift apart. Tested by
# tests/release/check-static-test.sh (`just test-release`).
#
# Usage: scripts/check-static.sh <binary>
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <binary>" >&2
  exit 2
fi
BIN="$1"

fail() {
  # GitHub Actions turns `::error::` lines into annotations; elsewhere the
  # prefix is plain text.
  if [ -n "${GITHUB_ACTIONS:-}" ]; then
    echo "::error::$BIN $1"
  else
    echo "error: $BIN $1" >&2
  fi
  exit 1
}

# readelf's output is read whole before it is searched: `readelf | grep -q`
# under pipefail fails with readelf's SIGPIPE (141) when grep stops at the
# first match, which an `if` takes for "no match".
[ -f "$BIN" ] || fail "does not exist"
file "$BIN"
program_headers="$(readelf --program-headers --wide "$BIN")" || fail "cannot be read by readelf"
dynamic="$(readelf --dynamic --wide "$BIN")" || fail "cannot be read by readelf"
file_header="$(readelf --file-header --wide "$BIN")" || fail "cannot be read by readelf"
section_headers="$(readelf --section-headers --wide "$BIN")" || fail "cannot be read by readelf"

if [[ "$program_headers" == *"program interpreter"* ]]; then
  fail "requests a dynamic loader"
fi
if [[ "$dynamic" == *"(NEEDED)"* ]]; then
  fail "depends on shared libraries"
fi
if ! [[ "$file_header" =~ Type:\ *DYN ]]; then
  fail "is not position-independent (static-pie)"
fi
if [[ "$section_headers" != *".symtab"* ]]; then
  fail "has no symbol table"
fi
ls -l "$BIN"
"$BIN" version
