#!/usr/bin/env bash
# Checks the two test registries of tests/registry/compose.yaml from this
# host: pushes a small local image into each with podman and reads its
# manifest back with skopeo. HTTP is anonymous; HTTPS needs the CA and the
# credentials, and must refuse an anonymous read. (podman, not skopeo,
# reads local storage: hosts that restrict unprivileged user namespaces
# with AppArmor let only podman unshare.)
#
#   tests/registry/check.sh [image]   default docker.io/library/alpine:3.20
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_data"
SRC="${1:-docker.io/library/alpine:3.20}"
CREDS="$(cat "$DIR/auth/credentials")"
export SUPPRESS_BOLTDB_WARNING=1
# skopeo reads a directory of CA files, named *.crt.
CERTS="$(mktemp -d)"
trap 'rm -rf "$CERTS"' EXIT
cp "$DIR/certs/ca.crt" "$CERTS/ca.crt"

digest_of() { skopeo inspect --raw "${@:2}" "$1" | sha256sum | cut -d' ' -f1; }

podman push -q --tls-verify=false "$SRC" docker://127.0.0.1:5000/check/probe:1
http="$(digest_of docker://127.0.0.1:5000/check/probe:1 --tls-verify=false)"
echo "http   127.0.0.1:5000  pushed and read back: sha256:$http"

podman push -q --cert-dir "$CERTS" --creds "$CREDS" "$SRC" docker://localhost:5443/check/probe:1
https="$(digest_of docker://localhost:5443/check/probe:1 --cert-dir "$CERTS" --creds "$CREDS")"
echo "https  localhost:5443  pushed and read back: sha256:$https"

if skopeo inspect --raw --cert-dir "$CERTS" docker://localhost:5443/check/probe:1 >/dev/null 2>&1; then
  echo "error: the HTTPS registry served an anonymous read" >&2
  exit 1
fi
echo "https  anonymous read refused"
[ "$http" = "$https" ] || { echo "error: the two copies differ" >&2; exit 1; }
echo "registries: ok"
