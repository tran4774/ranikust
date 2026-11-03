#!/usr/bin/env bash
# Checks that the x86_64 musl release binary is what ranikust ships
# (ADR-0020): static-pie, no program interpreter, no shared library, a symbol
# table kept for readable panic backtraces, and that it runs.
#
# Used by `just build-static`, CI's `static-build` job and the release
# workflow (ADR-0089), so the three cannot drift apart.
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

[ -f "$BIN" ] || fail "does not exist"
file "$BIN"
if readelf --program-headers --wide "$BIN" | grep -q 'program interpreter'; then
  fail "requests a dynamic loader"
fi
if readelf --dynamic --wide "$BIN" | grep -q '(NEEDED)'; then
  fail "depends on shared libraries"
fi
if ! readelf --file-header --wide "$BIN" | grep -q 'Type: *DYN'; then
  fail "is not position-independent (static-pie)"
fi
if ! readelf --section-headers --wide "$BIN" | grep -q '\.symtab'; then
  fail "has no symbol table"
fi
ls -l "$BIN"
"$BIN" version
