# RUN in its forms and with its mounts: shell and exec form, a heredoc and
# a `#!` heredoc, a cache mount kept across builds, a secret as a file and
# as a variable (ADR-0032), --network=none, and a RUN that changes nothing.
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
EOF
# What an earlier build left in the cache, then one more mark.
RUN --mount=type=cache,id=quick-run,target=/cache \
    sh -c 'cat /cache/marks > /seen 2>/dev/null || : > /seen; echo x >> /cache/marks'
# The secret's value is never written here: its sha256 is.
RUN --mount=type=secret,id=tok test "$(sha256sum < /run/secrets/tok | cut -d' ' -f1)" = 0010c18421c2d032793df4a17f2fa4021d76da9252c034d22b3a5e23e019dab8
RUN --mount=type=secret,id=tok,env=TOK test "${#TOK}" = 12 && test ! -e /run/secrets/tok
RUN test -z "${TOK:-}"
RUN --network=none test "$(ls /sys/class/net)" = lo
# Changes nothing: no layer, only its history entry (ADR-0058).
RUN true
CMD ["sh", "-c", "cat /shell /exec /heredoc /shebang | tr '\\n' ' '"]
