FROM a
COPY --chown=été a b
ENV café=crème
RUN echo 你好 \
  世界
