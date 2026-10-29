ARG BASE=alpine:3.20
ARG PLATFORM
FROM --platform=$PLATFORM ${BASE} AS Build
COPY --from=build --chown=1000:1000 --chmod=0644 --link a b /dst/
COPY --from=0 --parents --exclude=*.tmp --exclude="**/cache" src/ /app/
COPY --chown="user name":group -- --weird-name /x
COPY --link=false --parents=true ["a b", "c"] /d/
ADD --checksum=sha256:0123 --keep-git-dir=true --unpack=false https://example.com/a.tar /x/
ADD --keep-git-dir --unpack --exclude=a --exclude=b --link git@example.com:x/y.git /src
ADD --chown=$UID:$GID --chmod=$MODE a /b
RUN --mount=type=cache,target=/root/.cache,id=pip,sharing=locked,uid=1000,gid=1000,mode=0755 pip install x
RUN --mount=type=secret,id=token,env=TOKEN,required=true --mount=type=secret,target=/run/secrets/x,required echo
RUN --mount=type=ssh --mount=type=tmpfs,target=/tmp,size=64m --mount=type=bind,from=build,source=/out,target=/in,rw true
RUN --mount=target=/ctx,ro=false --mount=type=cache,target=/c,readonly=true --network=none --security=insecure ls
RUN --network=host true
RUN --device=nvidia.com/gpu=all,required --device=name=x true
RUN --mount=type=cache,target=/a,ro --mount=type=bind,target=/b,readwrite echo
RUN --mount="type=cache,\"target=/a,b\"" echo
RUN --mount=type=secret,src=/s,dst=/d,destination=/e echo
COPY --from=Build a b
