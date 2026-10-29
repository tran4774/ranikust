FROM a
ONBUILD RUN echo hi
ONBUILD COPY --from=build /a /b
onbuild add . /app
ONBUILD ENV A=1 B=2
