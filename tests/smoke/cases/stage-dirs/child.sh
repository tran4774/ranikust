# child.sh: run in the stage built FROM `hostile`; prints what it got.
yn() { if "$@"; then echo yes; else echo no; fi; }
echo "fifo $(yn test -p /fifo)"
echo "chr $(yn test -c /left-chr)"
echo "blk $(yn test -b /left-blk)"
echo "size $(stat -c %s /sparse)"
# 512-byte blocks: under 8 MiB allocated for 15 TiB.
echo "sparse $(yn test "$(stat -c %b /sparse)" -lt 16384)"
echo "mid $(dd if=/sparse bs=1 skip=1099511627776 count=3 2>/dev/null)"
echo "suid $(yn test -u /su) linked $(yn test /su -ef /su2)"
echo "links $(readlink /etc-root) $(readlink /up) $(readlink /pr)"
echo "newline $(cat "/$(printf 'n\nl')/f")"
