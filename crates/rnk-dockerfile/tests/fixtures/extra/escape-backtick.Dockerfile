# escape=`
FROM windows `
  AS w
RUN dir c:\ `
  && echo `"quoted`"
ENV PATH=C:\a;C:\b X=`$Y
COPY a\b c\d
RUN echo \\ `
