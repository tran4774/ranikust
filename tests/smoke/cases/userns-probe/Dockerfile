# Red team of the user namespace probe of --mode=auto (#74; T4, T7, T8,
# T9): a RUN sees no process, mount, descriptor, variable or directory of
# it (seen.sh).
ARG BASE=docker.io/library/alpine:3.20
FROM ${BASE}
COPY seen.sh /seen.sh
RUN sh /seen.sh
CMD ["cat", "/seen"]
