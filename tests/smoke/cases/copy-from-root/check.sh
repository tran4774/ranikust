# Runs inside the built image; prints `ok` or what is wrong.
set -u
bad=0
no() { echo "wrong: $*"; bad=1; }
[ "$(cat /all/app/main.js)" = js ] || no "/all/app/main.js"
[ -f /all/etc/alpine-release ] || no "/all/etc/alpine-release"
[ "$(readlink /all/to-state)" = /var/lib/ranikust ] || no "to-state"
[ -z "$(ls -A /all/var/lib/ranikust 2>/dev/null)" ] || no "the state directory was copied"
[ ! -e /all/ranikust ] || no "/ranikust was copied"
[ -z "$(ls -A /all/proc 2>/dev/null)" ] || no "/proc was copied"
[ "$bad" = 0 ] && echo ok
