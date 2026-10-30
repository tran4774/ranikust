# --reproducible: files written at build time (fixed contents, fresh
# times), a COPY from another stage and a heredoc, yet the same digest on
# every build and at every --jobs.
ARG BASE=docker.io/library/alpine:3.20
FROM ${BASE} AS side
RUN echo side > /side

FROM ${BASE}
COPY --from=side /side /side
RUN echo built > /built && touch /stamp && mkdir -p /d && echo f > /d/f
COPY <<EOF /note
reproducible
EOF
