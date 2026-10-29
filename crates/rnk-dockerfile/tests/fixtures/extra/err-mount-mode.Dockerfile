FROM a
RUN --mount=type=bind,target=/a,mode=0755 true
