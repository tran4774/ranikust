# syntax=docker/dockerfile:1.7
# check=error=true

# Doc comment for the stage
# base the image on alpine
FROM alpine AS base
# ARG doc
# VERSION the version to use
ARG VERSION
#
RUN true
  # indented comment
RUN true
