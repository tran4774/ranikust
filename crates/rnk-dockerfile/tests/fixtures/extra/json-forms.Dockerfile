FROM a
CMD ["a", "b c"]
CMD ['single']
CMD [ ]
ENTRYPOINT []
ENTRYPOINT ["x",]
RUN ["echo", "\u00e9\n"]
SHELL ["/bin/bash", "-c"]
VOLUME ["/a", "/b"]
VOLUME /c /d
CMD echo shell form
ENTRYPOINT exec x
RUN
CMD
