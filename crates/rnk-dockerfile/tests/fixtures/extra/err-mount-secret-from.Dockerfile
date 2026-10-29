FROM a
RUN --mount=type=secret,id=x,from=y true
