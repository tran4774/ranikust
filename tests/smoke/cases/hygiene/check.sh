# Runs inside the built image, as root; prints `ok` or what is wrong.
set -u
bad=0
no() { echo "wrong: $*"; bad=1; }
[ -f /run-saw-var-run-link ] || no "RUN did not see /var/run -> /run"
[ -L /var/run ] && [ "$(readlink -f /var/run)" = /run ] || no "/var/run is not a link to /run"
[ "$(stat -c %a /cd)" = 640 ] || no "--chmod=0640 on the copied directory: $(stat -c %a /cd)"
[ "$(stat -c %a /cd/sub)" = 640 ] || no "--chmod=0640 on a subdirectory: $(stat -c %a /cd/sub)"
[ "$(stat -c %a /cd/sub/f)" = 640 ] || no "--chmod=0640 on a file: $(stat -c %a /cd/sub/f)"
[ "$bad" = 0 ] && echo ok
