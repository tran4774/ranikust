# COPY and its options (ADR-0032): wildcards, one matching nothing (not an
# error), .dockerignore with an exception, --chown, --chmod octal and
# symbolic, a symlinked source (followed), a directory into a new
# destination, a heredoc.
ARG BASE=docker.io/library/alpine:3.20
FROM ${BASE}
COPY *.txt /w/
COPY *.nomatch /w/
COPY . /ctx/
COPY --chown=1000:1000 a.txt /o/a
COPY --chmod=640 b.txt /m/b
COPY --chmod=u=rwx,go=r a.txt /m/sym
COPY link /l/link
COPY dir /newdir/
COPY <<EOF /h/file.txt
from a heredoc
EOF
COPY check.sh /check.sh
CMD ["sh", "/check.sh"]
