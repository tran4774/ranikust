# The end-to-end build of `just smoke` (T1) and `just test-vm smoke` (T2):
# Phase 1 instructions over two stages. Each RUN checks what it can see;
# tests/smoke/lib.bash checks the image.
ARG BASE=docker.io/library/alpine:3.20

FROM ${BASE} AS build
ARG GREETING=hello
WORKDIR /work
COPY app.txt ./
COPY *.conf conf/
# .dockerignore keeps ignored.txt out of the context.
COPY . /ctx/
RUN test -f /ctx/app.txt && test ! -e /ctx/ignored.txt && test -f conf/a.conf && test -f conf/b.conf
RUN echo "$GREETING from ranikust" > hello && cat app.txt >> hello
RUN --mount=type=cache,target=/cache echo cached > /cache/stamp && ls /cache > cache-list
# Phase 1 acceptance: `--network=none` sees only loopback, and nothing
# outside answers.
RUN --network=none test "$(ls /sys/class/net)" = lo && test "$(tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' ')" = lo
RUN --network=none sh -c '! wget -q -T 3 -O /dev/null http://1.1.1.1/'
# Loopback is up (IFF_UP), also without CAP_NET_ADMIN (ADR-0033).
RUN --network=none test $(( $(cat /sys/class/net/lo/flags) & 1 )) = 1
# The secret's value never appears in the Dockerfile, so that it may be
# searched for in logs and blobs: this is its sha256.
RUN --mount=type=secret,id=token test "$(sha256sum < /run/secrets/token | cut -d' ' -f1)" = 4e738ca5563c06cfd0018299933d58db1dd8bf97f6973dc99bf6cdc64b5550bd
# BuildKit's secret file: 0400 and root's unless asked; env= alone gives a
# variable of this RUN and no file (ADR-0032).
RUN --mount=type=secret,id=token test "$(stat -c %a:%u:%g /run/secrets/token)" = 400:0:0
RUN --mount=type=secret,id=token,target=/s,mode=0440,uid=1000,gid=1000 test "$(stat -c %a:%u:%g /s)" = 440:1000:1000
RUN --mount=type=secret,id=token,env=TOKEN test "${#TOKEN}" = 6 && test ! -e /run/secrets/token
RUN test -z "${TOKEN:-}"
# A cache's owner and mode apply when it is made; locked and private
# sharing (ADR-0032).
RUN --mount=type=cache,target=/c2,sharing=locked,uid=1000,mode=0700 test "$(stat -c %a:%u /c2)" = 700:1000
RUN --mount=type=cache,target=/c3,sharing=private touch /c3/x
RUN <<EOF
set -e
mkdir -p /out
cp /work/hello /work/cache-list /out/
EOF

# Independent of `build`: with --jobs above 1 both run at once.
FROM ${BASE} AS other
RUN echo other > /other

FROM ${BASE}
LABEL org.example.test=e2e
ENV APP_HOME=/srv/app
WORKDIR $APP_HOME
COPY --from=build /out/ ./
COPY --from=other /other ./
# Changes nothing: no layer, only history (`smoke_check_no_empty_layer`, ADR-0058).
RUN true
COPY --chmod=755 <<EOF /usr/local/bin/show
#!/bin/sh
cat /srv/app/hello
EOF
RUN adduser -D -H app && chown app "$APP_HOME"
USER app
EXPOSE 8080
CMD ["show"]
