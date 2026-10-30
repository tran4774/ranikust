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
RUN --network=none test "$(ls /sys/class/net)" = lo
RUN --mount=type=secret,id=token test "$(cat /run/secrets/token)" = s3cr3t
RUN <<EOF
set -e
mkdir -p /out
cp /work/hello /work/cache-list /out/
EOF

FROM ${BASE}
LABEL org.example.test=e2e
ENV APP_HOME=/srv/app
WORKDIR $APP_HOME
COPY --from=build /out/ ./
COPY --chmod=755 <<EOF /usr/local/bin/show
#!/bin/sh
cat /srv/app/hello
EOF
RUN adduser -D -H app && chown app "$APP_HOME"
USER app
EXPOSE 8080
CMD ["show"]
