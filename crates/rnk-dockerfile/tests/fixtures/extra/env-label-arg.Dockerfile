ARG A
ARG B=1 C="two words" D=
ARG E='single'
FROM x
ENV legacy  value with  spaces
ENV K1=v1 K2="v 2" K3=v\ 3 K4=
ENV Q='a b' R="$A"
LABEL "com.example"="x y" other=1
LABEL legacy label value
ARG F G=g
MAINTAINER Someone <someone at example.com>
expose 80 443/udp 8080/tcp 1000-2000
EXPOSE $PORT
USER 1000:1000
WORKDIR /a b
STOPSIGNAL SIGTERM
VOLUME /v
