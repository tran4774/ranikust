#!/bin/sh
# Regenerates rnk-dockerfile's BuildKit goldens with BuildKit v0.34.0 in a Go
# container. Needs podman (or ENGINE=docker); nothing is installed on the host.
# See README.md.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
TESTS=$(cd "$HERE/.." && pwd)
ENGINE=${ENGINE:-podman}
# golang 1.26.9, the image experiment 04 used.
GO_IMG=${GO_IMG:-docker.io/library/golang@sha256:cf7e928cb9edc2ed8b3cc6fe77ee1ee30b9fea28149564c8840097321a7600ea}
# A named volume caches Go modules between runs.
GO_MOD_VOLUME=${GO_MOD_VOLUME:-rk-gomod}

"$ENGINE" run --rm \
  -v "$TESTS:/tests" \
  -v "$GO_MOD_VOLUME:/go/pkg/mod" \
  -w /tests/golden-gen \
  -e GOFLAGS=-mod=mod \
  "$GO_IMG" sh -euc '
    go build -o /tmp/golden-gen .
    rm -rf /tests/golden/kaniko
    mkdir -p /tests/golden/kaniko
    /tmp/golden-gen dockerfile /tests/golden/kaniko /tests/fixtures/kaniko/*
    rm -rf /tests/golden/expand
    mkdir -p /tests/golden/expand
    /tmp/golden-gen expand /tests/golden/expand /tests/fixtures/kaniko/*
    rm -rf /tests/golden/buildkit
    mkdir -p /tests/golden/buildkit
    for d in /tests/fixtures/buildkit/parser/testfiles/* /tests/fixtures/buildkit/parser/testfiles-negative/*; do
      cp "$d/Dockerfile" "/tmp/$(basename "$(dirname "$d")")_$(basename "$d")"
    done
    /tmp/golden-gen dockerfile /tests/golden/buildkit /tmp/testfiles*
    rm -rf /tests/golden/extra
    mkdir -p /tests/golden/extra
    /tmp/golden-gen dockerfile /tests/golden/extra /tests/fixtures/extra/*
    /tmp/golden-gen lex /tests/golden/lex.json
  '
