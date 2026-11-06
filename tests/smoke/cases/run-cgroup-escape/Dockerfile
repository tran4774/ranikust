# The probe of case.sh; its report is the image's /report.
ARG BASE=docker.io/library/alpine:3.20
FROM ${BASE} AS probe
RUN apk add --no-cache -q python3
COPY probe.py /
RUN python3 -I /probe.py /report

FROM ${BASE}
COPY --from=probe /report /report
CMD ["cat", "/report"]
