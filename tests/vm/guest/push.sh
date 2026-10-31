#!/usr/bin/env bash
# Runs in the T2 VM: the push acceptance of Phase 1. One `ranikust build`, as
# real root, pushes to three registries and also writes a Docker tarball and
# an OCI layout:
#   loopback   registry:2 in the VM's Docker on 127.0.0.1:5000: plain HTTP,
#              anonymous, unasked (ADR-0032, Q-065)
#   http       the host's `tests/registry` HTTP registry, over the network
#              as 10.0.2.2:5000, with `--insecure-registry`
#   https      the host's HTTPS registry, 10.0.2.2:5443: TLS from a
#              self-signed CA through `--registry-certificate`, basic auth
#              from a docker config.json
# Each pushed manifest is read back and must hash to the digest ranikust
# printed; the loopback image is pulled and run by Docker; the tarball and
# the OCI layout are loaded into Docker and run, and every blob of the
# layout is checked against its name.
set -euo pipefail
cd "$HOME/rnk"

REG="${RANIKUST_VM_IMAGE_REGISTRY:-mirror.gcr.io}"
RNK="$HOME/rnk/bin/ranikust"
CA="$HOME/rnk/registry/ca.crt"
CREDS="$(cat "$HOME/rnk/registry/credentials")"
W=/var/tmp/rnk-push
REPO=rnk/push
TAG="push-$(date +%s)"

sudo rm -rf "$W" && sudo mkdir -m 700 -p "$W/ctx" "$W/work" "$W/docker"
printf 'FROM %s/library/alpine:3.20\nRUN echo "pushed by ranikust" > /pushed\nCMD ["cat", "/pushed"]\n' "$REG" |
  sudo tee "$W/ctx/Dockerfile" >/dev/null
printf '{"auths":{"10.0.2.2:5443":{"auth":"%s"}}}\n' "$(printf '%s' "$CREDS" | base64 -w0)" |
  sudo tee "$W/docker/config.json" >/dev/null

echo "=== loopback registry in the VM"
for i in 1 2 3 4 5; do
  sudo docker pull -q "$REG/library/registry:2" >/dev/null && break
  [ "$i" = 5 ] && { echo "error: docker pull registry:2 failed 5 times" >&2; exit 1; }
  sleep $((i * 5))
done
sudo docker rm -f rnk-push-registry >/dev/null 2>&1 || true
sudo docker run -d --name rnk-push-registry -p 127.0.0.1:5000:5000 "$REG/library/registry:2" >/dev/null
trap 'sudo docker rm -f rnk-push-registry >/dev/null 2>&1 || true' EXIT
for _ in $(seq 30); do curl -fsS -o /dev/null http://127.0.0.1:5000/v2/ && break; sleep 1; done

echo "=== ranikust build: three destinations, a tarball and an OCI layout"
digest="$(sudo env DOCKER_CONFIG="$W/docker" "$RNK" --context "$W/ctx" --state-dir "$W/work" \
  --destination "127.0.0.1:5000/$REPO:$TAG" \
  --destination "10.0.2.2:5000/$REPO:$TAG" --insecure-registry 10.0.2.2:5000 \
  --destination "10.0.2.2:5443/$REPO:$TAG" --registry-certificate "10.0.2.2:5443=$CA" \
  --tar-path "$W/image.tar" --oci-layout-path "$W/layout" | tail -1)"
[[ "$digest" == sha256:* ]] || { echo "error: no digest printed: $digest" >&2; exit 1; }
echo "ranikust printed $digest"

ACCEPT='application/vnd.oci.image.manifest.v1+json,application/vnd.docker.distribution.manifest.v2+json'
manifest_digest() { # base-url, curl options...
  local base="$1"; shift
  echo "sha256:$(curl -fsS "$@" -H "Accept: $ACCEPT" "$base/v2/$REPO/manifests/$TAG" | sha256sum | cut -d' ' -f1)"
}
check() { # name, got
  [ "$2" = "$digest" ] || { echo "error: $1: the registry holds $2, not $digest" >&2; exit 1; }
  echo "$1: manifest read back, $2"
}
check "loopback 127.0.0.1:5000" "$(manifest_digest http://127.0.0.1:5000)"
check "http 10.0.2.2:5000" "$(manifest_digest http://10.0.2.2:5000)"
check "https 10.0.2.2:5443" "$(manifest_digest https://10.0.2.2:5443 --cacert "$CA" -u "$CREDS")"
code="$(curl -sS -o /dev/null -w '%{http_code}' --cacert "$CA" -H "Accept: $ACCEPT" "https://10.0.2.2:5443/v2/$REPO/manifests/$TAG")"
[ "$code" = 401 ] || { echo "error: the HTTPS registry answered $code to an anonymous read" >&2; exit 1; }
echo "https 10.0.2.2:5443: an anonymous read is refused (401)"

echo "=== Docker runs what was pushed and written"
run_image() { # name, image
  local got
  got="$(sudo docker run --rm "$2")"
  [ "$got" = "pushed by ranikust" ] || { echo "error: $1: the image printed '$got'" >&2; exit 1; }
  sudo docker rmi -f "$2" >/dev/null
  echo "$1: runs"
}
sudo docker pull -q "127.0.0.1:5000/$REPO:$TAG" >/dev/null
run_image "pulled from the loopback registry" "127.0.0.1:5000/$REPO:$TAG"

loaded="$(sudo docker load -i "$W/image.tar" | sed -nE 's/^Loaded image( ID)?: //p' | tail -1)"
run_image "--tar-path loaded by docker ($loaded)" "$loaded"

# Every blob of the OCI layout must hash to its name, and the index must
# point at the manifest that was pushed.
sudo python3 -I - "$W/layout" "$digest" <<'PY'
import hashlib, json, os, sys
root, want = sys.argv[1], sys.argv[2]
assert json.load(open(os.path.join(root, "oci-layout")))["imageLayoutVersion"] == "1.0.0"
index = json.load(open(os.path.join(root, "index.json")))
digests = [m["digest"] for m in index["manifests"]]
assert want in digests, f"index.json lists {digests}, not {want}"
blobs = os.path.join(root, "blobs", "sha256")
for name in os.listdir(blobs):
    h = hashlib.sha256()
    with open(os.path.join(blobs, name), "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    assert h.hexdigest() == name, f"blob {name} hashes to {h.hexdigest()}"
manifest = json.load(open(os.path.join(blobs, want.split(":")[1])))
for d in [manifest["config"]] + manifest["layers"]:
    assert os.path.exists(os.path.join(blobs, d["digest"].split(":")[1])), d["digest"]
print(f"--oci-layout-path: valid, {len(os.listdir(blobs))} blobs, index -> {want}")
PY
sudo tar -C "$W/layout" -cf "$W/layout.tar" .
loaded="$(sudo docker load -i "$W/layout.tar" | sed -nE 's/^Loaded image( ID)?: //p' | tail -1)"
run_image "--oci-layout-path loaded by docker ($loaded)" "$loaded"

sudo rm -rf "$W"
echo "push: ok"
