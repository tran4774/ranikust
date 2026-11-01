# Red team (T7, T11, T21, ADR-0057): RUN steps that attack a `Direct` RUN
# (kaniko mode on the container's `/`) and fail the build when a defence
# gives way. Built by case.sh in kaniko mode only.
ARG BASE=docker.io/library/alpine:3.20
FROM ${BASE}
# A `#!` heredoc with `<<-` and a quoted delimiter: run from the sealed
# memfd, which the script can neither append to, truncate, nor rewrite
# through /proc; nothing of it is named in the root.
RUN <<-"EOF"
	#!/bin/sh
	set -e
	n=${0##*/}
	case "$0" in /dev/fd/*) ;; *) echo "script named $0" >&2; exit 1 ;; esac
	! ( printf x >> "$0" ) 2>/dev/null
	! ( : > "$0" ) 2>/dev/null
	! ( exec 5<>/proc/self/fd/$n; printf y >&5 ) 2>/dev/null
	! ( printf z 1<>/dev/fd/$n ) 2>/dev/null
	test "$(head -1 "$0")" = '#!/bin/sh'
	test ! -e /dev/pipes
	echo chomped > /chomped
EOF
# Interpreters with arguments and through env.
RUN <<EOF
#!/bin/sh -eu
echo "${0%/*}" > /interp-args
EOF
RUN <<EOF
#!/usr/bin/env sh
echo env > /interp-env
EOF
# Several heredocs in one RUN: the shell reads them, no script.
RUN cat <<A > /two-a && cat <<B > /two-b
first
A
second
B
# The descriptors a RUN holds, in shell form and as a script: its own
# pipes, /dev/null, its script; nothing of ranikust's (state directory,
# outputs, its terminal or log).
RUN for f in /proc/$$/fd/*; do t="$(readlink "$f")"; case "$t" in pipe:*|/dev/null|/proc/*) ;; *) echo "fd ${f##*/} -> $t" >&2; exit 1 ;; esac; done
RUN <<EOF
#!/bin/sh
for f in /proc/$$/fd/*; do
  t="$(readlink "$f")"
  case "$t" in pipe:*|/dev/null|/proc/*|/memfd:ranikust-heredoc*) ;; *) echo "fd ${f##*/} -> $t" >&2; exit 1 ;; esac
done
EOF
# A non-root USER owns its streams: /proc/self/fd/1 and 2 reopen.
USER nobody
RUN echo out > /proc/self/fd/1 && echo err > /proc/self/fd/2 && test "$(stat -L -c %u /proc/$$/fd/1)" = 65534
USER root
# A flood of output, relayed whole; then streams closed early.
RUN yes rnk-flood | head -c 1048576
RUN exec >&- 2>&-; sleep 1; echo closed > /closed
# A background writer and a setsid loop are gone before the next RUN.
RUN (sleep 30; echo late > /late) & setsid sh -c 'while :; do echo rnk-spam; sleep 0.01; done; : rnk-bg' & sleep 0.3
RUN ! pgrep -f 'rnk-b[g]' && ! pgrep -f 'sleep 3[0]' && test ! -e /late
# Secret variables, however the mount is spelled or expanded.
ARG T=secret
RUN --mount=type=Secret,id=tok,env=TOK test "${#TOK}" = 12
RUN --mount=TYPE=$T,ID=tok,ENV=TOK,target=/x,target= test "${#TOK}" = 12 && test ! -e /x
RUN test -z "${TOK:-}"
CMD ["sh", "-c", "cat /chomped /interp-args /interp-env /two-a /two-b /closed | tr '\\n' ' '"]
