FROM scratch
FROM a AS one
FROM one as Two
FROM --platform=linux/amd64 b aS three
FROM $X
from c AS x.y-z_1
