FROM a
RUN --mount=type=cache,target=/a,foo=bar true
