# Red team (T7, ADR-0082): a script whose interpreter is /proc/self/exe,
# run as the RUN's command itself. The kernel opens the interpreter while
# the RUN child is still a copy of ranikust, so ranikust's binary runs, with
# the script as its first argument; its command line must refuse that. The
# build fails.
ARG BASE=docker.io/library/alpine:3.20
FROM ${BASE}
RUN printf '#!/proc/self/exe\n' > /x && chmod 755 /x
RUN ["/x"]
