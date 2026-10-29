#!/bin/sh
# Regenerates src/unicode_tables.rs and tests/golden/*.txt from the Go code
# rnk-ignore must match. Needs podman (or ENGINE=docker) and network access
# for the Go modules; nothing is installed on the host.
#
#   crates/rnk-ignore/gen/gen.sh
#
# The image is pinned by digest (go1.26.9); module downloads go to a
# throwaway volume that is removed afterwards. GEN_SCALE=N writes N times
# more random cases, for a one-off deeper comparison (`cargo test -p
# rnk-ignore --test golden`); commit only the default (GEN_SCALE unset).
# Go's text/scanner prints "invalid UTF-8 encoding" and "invalid character
# NUL" for some patterns; that output is expected.
set -eu
CRATE=$(cd "$(dirname "$0")/.." && pwd)
ENGINE=${ENGINE:-podman}
GO_IMG=${GO_IMG:-docker.io/library/golang@sha256:cf7e928cb9edc2ed8b3cc6fe77ee1ee30b9fea28149564c8840097321a7600ea}
VOLUME=rnk-ignore-gen-gomod
export SUPPRESS_BOLTDB_WARNING=1

trap '"$ENGINE" volume rm -f "$VOLUME" >/dev/null 2>&1 || true' EXIT
"$ENGINE" run --rm \
  -v "$CRATE:/crate" \
  -v "$VOLUME:/go/pkg/mod" \
  -e GEN_SCALE="${GEN_SCALE:-}" \
  -w /crate/gen \
  "$GO_IMG" \
  sh -c 'go run . /crate && gofmt -l . | (! grep .)'
