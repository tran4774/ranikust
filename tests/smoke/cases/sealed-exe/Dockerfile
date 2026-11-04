# Red team (ADR-0101, T7, T21): a RUN reaches neither the binary ranikust
# and its init run from nor their descriptors (reach.sh).
ARG BASE=docker.io/library/alpine:3.20
FROM ${BASE}
COPY reach.sh /reach.sh
RUN sh /reach.sh
CMD ["cat", "/reached"]
