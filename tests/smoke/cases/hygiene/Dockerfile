# Fixes from the issue mining (ADR-0052): the base's /var/run -> /run
# symlink is kept (D4), an octal --chmod without x applies to directories
# as BuildKit (D3), and no-op steps each add the same empty layer, as
# BuildKit (Q-131) [open: Q-385].
ARG BASE=docker.io/library/alpine:3.20
FROM ${BASE}
RUN [ -L /var/run ] && [ "$(readlink -f /var/run)" = /run ] && touch /run-saw-var-run-link
COPY --chmod=0640 dir /cd/
RUN true
RUN true
WORKDIR /tmp
WORKDIR /tmp
COPY check.sh /check.sh
CMD ["sh", "/check.sh"]
