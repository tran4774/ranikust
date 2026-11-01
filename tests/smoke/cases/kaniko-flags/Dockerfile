# The kaniko-mode flags tested for their meaning (Phase 2 criterion 8):
# `--ignore-path` keeps what a RUN writes below it out of every layer, and
# `--snapshot-mode=time` misses a rewrite of the same size and mtime that
# `full` catches, as in kaniko.
ARG BASE
FROM ${BASE}
RUN mkdir -p /var/cache/app /opt && echo cached >/var/cache/app/f && echo kept >/opt/kept
RUN printf AAAA >/opt/same && touch -d '2001-01-01 00:00:00' /opt/same
RUN printf BBBB >/opt/same && touch -d '2001-01-01 00:00:00' /opt/same
CMD ["cat", "/opt/same"]
