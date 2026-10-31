# Runs inside the built image; prints `ok` or what is wrong.
set -u
bad=0
no() { echo "wrong: $*"; bad=1; }
[ "$(cat /app/a)" = a ] || no "/app/a"
[ "$(cat /copied-a)" = a ] || no "/copied-a"
[ "$(cat /b)" = b ] || no "/b"
[ -z "$(ls -A /var/lib/ranikust 2>/dev/null)" ] || no "the state directory reached the image"
[ ! -e /ranikust ] || no "/ranikust reached the image"
if grep -rqs "$1" / --exclude-dir=proc --exclude-dir=sys 2>/dev/null; then no "the secret reached the image"; fi
[ "$bad" = 0 ] && echo ok
