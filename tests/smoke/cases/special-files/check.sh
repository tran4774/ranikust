# Runs inside the built image: the kind of each copied entry (p FIFO,
# s socket, c character device, f regular file, none), then what the RUNs
# saw of their own mknod.
set -u
kind() {
  if [ -p "$1" ]; then echo p
  elif [ -S "$1" ]; then echo s
  elif [ -c "$1" ]; then echo c
  elif [ -b "$1" ]; then echo b
  elif [ -f "$1" ]; then echo f
  elif [ -e "$1" ] || [ -L "$1" ]; then echo other
  else echo none; fi
}
for p in /ctx/fifo /ctx/sock /ctx/d/e/fifo /ctx/d/e/sock /ctx/real/fifo /lone-fifo /lone-sock \
  /via/fifo /several/fifo /several/sock /from-stage/d/fifo /from-stage/chr /lone-stage-fifo \
  /lone-stage-chr /made; do
  echo "$p $(kind "$p")"
done
echo "keep $(cat /ctx/keep.txt /ctx/d/e/keep /from-stage/keep 2>&1 | tr '\n' ' ')"
cat /mknod.txt /open.txt
