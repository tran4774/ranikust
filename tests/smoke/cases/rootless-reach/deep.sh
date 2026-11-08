# Makes under $1 a tree deeper than PATH_MAX: three chains of 1000
# directories, nested by rename, the lower two owned by a mapped user, the
# lowest searchable by no one, a file at the bottom.
set -e
d="$(printf 'd/%.0s' $(seq 1000))"
cd "$1"
for k in k1 k2 k3; do mkdir -p "$k/$d"; done
echo x >"k3/${d}f"
chown -R 1234:1234 k2 k3
chmod 0 k3/d
mv k3 "k2/$d"
mv k2 "k1/$d"
