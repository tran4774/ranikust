# The RUN forms a `Direct` RUN (kaniko mode on the container's `/`, where
# nothing can be mounted) gives (ADR-0057): shell and exec form, a heredoc
# and a `#!` heredoc, run from an anonymous file the interpreter reads as
# /dev/fd/<n>, a secret as a variable, a non-root USER writing the standard
# streams (kaniko#2928), and a RUN that changes nothing. Dockerfile keeps
# the mounts and --network=none, which are refused here (case.sh).
ARG BASE=docker.io/library/alpine:3.20
FROM ${BASE}
RUN echo shell > /shell
RUN ["/bin/sh", "-c", "echo exec > /exec"]
RUN <<EOF
echo heredoc > /heredoc
EOF
RUN <<EOF
#!/bin/sh
echo shebang > /shebang
case "$0" in /dev/fd/*) ;; *) echo "script named $0" >&2; exit 1 ;; esac
EOF
RUN --mount=type=secret,id=tok,env=TOK test "${#TOK}" = 12 && test ! -e /run/secrets/tok
RUN test -z "${TOK:-}"
USER nobody
RUN echo to-stdout > /dev/stdout && echo to-stderr > /dev/stderr
USER root
# Changes nothing: still a layer, an empty one.
RUN true
CMD ["sh", "-c", "cat /shell /exec /heredoc /shebang | tr '\\n' ' '"]
