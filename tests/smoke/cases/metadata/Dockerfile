# Instructions that only change the image config, checked against what
# BuildKit writes (ADR-0032): ARG before FROM, ARG/ENV expansion, WORKDIR,
# USER name:group, LABEL, EXPOSE, STOPSIGNAL, HEALTHCHECK, SHELL,
# ENTRYPOINT and CMD. `--target shellform`: a shell-form ENTRYPOINT clears
# the CMD inherited from its base stage (Q-130).
ARG BASE=docker.io/library/alpine:3.20
ARG VERSION=1.2

FROM ${BASE} AS withcmd
CMD ["inherited"]

FROM withcmd AS shellform
ENTRYPOINT echo shell-entry

FROM ${BASE}
ARG VERSION
ENV APP=/opt/app VER=v${VERSION}
WORKDIR $APP
WORKDIR sub
RUN test "$(pwd)" = /opt/app/sub
RUN addgroup -S grp && adduser -S -G grp usr
USER usr:grp
RUN test "$(id -un):$(id -gn)" = usr:grp
LABEL org.example.name=meta org.example.ver="$VER"
EXPOSE 8080 53/udp 9000-9001/tcp
STOPSIGNAL SIGQUIT
HEALTHCHECK --interval=30s --timeout=3s --retries=2 CMD ["true"]
SHELL ["/bin/sh", "-ec"]
RUN case "$-" in *e*) ;; *) exit 1 ;; esac
ENTRYPOINT ["/bin/echo", "ep"]
CMD ["cmd"]
