# Runs inside the built image; prints `ok` or what is wrong.
set -u
bad=0
no() { echo "wrong: $*"; bad=1; }
[ "$(ls /w | tr '\n' ' ')" = "a.txt b.txt " ] || no "/w holds $(ls /w | tr '\n' ' ')"
[ -f /ctx/a.txt ] && [ -f /ctx/keep.log ] || no "/ctx lacks a kept file"
[ ! -e /ctx/ignored.log ] || no ".dockerignore did not keep ignored.log out"
[ "$(stat -c %u:%g /o/a)" = 1000:1000 ] || no "--chown: $(stat -c %u:%g /o/a)"
[ "$(stat -c %a /m/b)" = 640 ] || no "--chmod=640: $(stat -c %a /m/b)"
[ "$(stat -c %a /m/sym)" = 744 ] || no "--chmod=u=rwx,go=r: $(stat -c %a /m/sym)"
[ -f /l/link ] && [ ! -L /l/link ] && [ "$(cat /l/link)" = A ] || no "the symlinked source was not followed"
[ "$(cat /newdir/x)" = x ] && [ "$(cat /newdir/sub/y)" = y ] && [ ! -e /newdir/dir ] ||
  no "a directory's contents did not land in /newdir"
[ "$(cat /h/file.txt)" = "from a heredoc" ] || no "heredoc: $(cat /h/file.txt)"
[ "$bad" = 0 ] && echo ok
