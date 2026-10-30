#!/usr/bin/env bash
# Makes what tests/registry/compose.yaml needs, under tests/registry/_data:
# a CA and a server certificate for the HTTPS registry, an htpasswd file for
# its basic auth, and the data directories. Existing files are kept.
#
#   certs/ca.crt              the CA a client trusts (--registry-certificate)
#   certs/server.{crt,key}    SANs: localhost, 127.0.0.1, ::1, 10.0.2.2 (the
#                             host as the T2 VM sees it) and this host's name
#   auth/htpasswd             user `ranikust`, bcrypt
#   auth/credentials          `ranikust:<password>`, for clients
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_data"
mkdir -p "$DIR"/http "$DIR"/https "$DIR"/certs "$DIR"/auth

cd "$DIR/certs"
if [ ! -f ca.crt ]; then
  openssl req -x509 -newkey rsa:3072 -nodes -days 3650 -sha256 \
    -subj "/CN=ranikust test registry CA" -keyout ca.key -out ca.crt 2>/dev/null
fi
if [ ! -f server.crt ]; then
  cat >server.ext <<EXT
basicConstraints = CA:FALSE
keyUsage = digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = DNS:localhost, DNS:$(hostname), IP:127.0.0.1, IP:::1, IP:10.0.2.2
EXT
  openssl req -newkey rsa:3072 -nodes -sha256 -subj "/CN=localhost" \
    -keyout server.key -out server.csr 2>/dev/null
  openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
    -days 825 -sha256 -extfile server.ext -out server.crt 2>/dev/null
  rm -f server.csr server.ext
fi
# The registry runs as root in a rootless container (an unprivileged uid on
# the host), so it needs to read the key.
chmod 0644 ca.crt server.crt server.key

cd "$DIR/auth"
if [ ! -f htpasswd ]; then
  pass="$(openssl rand -hex 16)"
  # The registry reads bcrypt only; python3's bcrypt makes it without httpd.
  python3 -I -c 'import bcrypt, sys
print("ranikust:" + bcrypt.hashpw(sys.argv[1].encode(), bcrypt.gensalt()).decode())' \
    "$pass" >htpasswd
  printf 'ranikust:%s\n' "$pass" >credentials
  chmod 0600 credentials
fi
echo "registry data in $DIR"
