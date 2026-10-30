# FROM scratch: a root built in another stage, an exec-form ENTRYPOINT, and
# the default PATH a config without one gets (ADR-0032, Q-185).
ARG BASE=docker.io/library/alpine:3.20
FROM ${BASE} AS tools
RUN mkdir -p /rootfs/bin /rootfs/lib && \
    cp /bin/busybox /rootfs/bin/ && cp /lib/ld-musl-*.so.1 /rootfs/lib/ && \
    echo "static" > /rootfs/hello

FROM scratch
COPY --from=tools /rootfs/ /
ENTRYPOINT ["/bin/busybox", "cat", "/hello"]
