# Red team (ADR-0052 D1, D4; threat model T1, T4), kaniko mode: built twice
# in one container holding a volume below /run, reached through alpine's
# /var/run -> /run, and one on /srv. The second build empties / before its
# base; neither may touch the volumes or the runtime's /run/.containerenv,
# and no layer may hold them.
ARG BASE=docker.io/library/alpine:3.20
FROM ${BASE}
RUN [ -L /var/run ] && [ -f /run/.containerenv ] && \
    [ -s /var/run/vol/token ] && [ -s /srv/vol/src ]
RUN [ ! -e /left-by-an-earlier-build ] && touch /left-by-an-earlier-build
RUN rm -f /etc/motd
RUN true
