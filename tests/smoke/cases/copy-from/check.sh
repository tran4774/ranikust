# Runs inside the built image; prints `ok` or what is wrong.
set -u
bad=0
no() { echo "wrong: $*"; bad=1; }
[ "$(stat -c %a /app/suid)" = 4755 ] || no "setuid: $(stat -c %a /app/suid)"
[ "$(stat -c %i /app/hard.js)" = "$(stat -c %i /app/dist/main.js)" ] || no "the hardlink was split"
[ "$(stat -c %u:%g:%a /app/private)" = 1000:1000:700 ] || no "private: $(stat -c %u:%g:%a /app/private)"
[ "$(cat /app/private/key)" = k ] || no "private/key"
[ "$(readlink /to-state)" = /var/lib/ranikust ] || no "to-state: $(readlink /to-state)"
[ "$(readlink /to-root)" = / ] || no "to-root: $(readlink /to-root)"
[ -z "$(ls -A /var/lib/ranikust 2>/dev/null)" ] || no "the state directory reached the image"
[ ! -e /ranikust ] || no "/ranikust reached the image"
[ "$(cat /via-link.js)" = js ] || no "via-link.js: $(cat /via-link.js)"
[ ! -e /app/null ] || [ "$(stat -c %F:%t:%T /app/null)" = "character special file:1:3" ] ||
  no "the device node: $(stat -c %F:%t:%T /app/null)"
[ "$(cat /b)" = b ] || no "b: $(cat /b)"
[ "$bad" = 0 ] && echo ok
