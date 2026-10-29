FROM a
RUN echo a \
    # interior comment

    b \
  \
    c
RUN echo trailing\\
RUN echo next
ENV A=1 \
    B=2
LABEL a=b \
	c=d
    RUN    indented   
RUN echo end \