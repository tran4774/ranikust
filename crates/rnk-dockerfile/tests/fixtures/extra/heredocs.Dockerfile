FROM alpine AS base
RUN <<EOF
echo hello
EOF
RUN <<-EOT bash -ex
	echo tabbed
		echo more
	EOT
RUN cat <<A > /a && cat <<"B" > /b
one $HOME
A
two $HOME
B
COPY <<file1 <<'file2' /dest/
content 1 ${X}
file1
content 2 ${X}
file2
COPY --chmod=755 <<-EOF /usr/local/bin/run.sh
	#!/bin/sh
	echo run
EOF
RUN 3<<EOF cat <&3
fd three
EOF
RUN <<EOF
#!/usr/bin/env python3
print("hi")
EOF
RUN echo "<<NOTHEREDOC" && echo '<<ALSO'
RUN ["cat", "<<EOF"]
ONBUILD RUN <<EOF
echo onbuild heredoc
EOF
RUN <<EOF > /dev/null
EOF
